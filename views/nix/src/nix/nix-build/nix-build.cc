#include <cstring>
#include <fstream>
#include <iostream>
#include <filesystem>
#include <regex>
#include <sstream>
#include <vector>
#include <optional>
#include <map>

#include <nlohmann/json.hpp>

#include "nix/util/current-process.hh"
#include "nix/store/parsed-derivations.hh"
#include "nix/store/derivation-options.hh"
#include "nix/store/store-open.hh"
#include "nix/store/local-fs-store.hh"
#include "nix/store/globals.hh"
#include "nix/store/realisation.hh"
#include "nix/store/derivations.hh"
#include "nix/main/shared.hh"
#include "nix/store/path-with-outputs.hh"
#include "nix/expr/eval.hh"
#include "nix/expr/eval-inline.hh"
#include "nix/expr/get-drvs.hh"
#include "nix/cmd/common-eval-args.hh"
#include "nix/expr/attr-path.hh"
#include "nix/cmd/legacy.hh"
#include "nix/util/users.hh"
#include "nix/cmd/network-proxy.hh"
#include "nix/cmd/compatibility-settings.hh"
#include "nix/util/fun.hh"
#include "nix/expr/rust-eval-refusal.hh"
#include "man-pages.hh"
#include "nix/cmd/rust-eval-session.hh"

using namespace nix;
using namespace std::string_literals;

extern char ** environ __attribute__((weak));

/* Recreate the effect of the perl shellwords function, breaking up a
 * string into arguments like a shell word, including escapes
 */
static std::vector<std::string> shellwords(std::string_view s)
{
    std::regex whitespace("^\\s+");
    auto begin = s.cbegin();
    std::vector<std::string> res;
    std::string cur;

    enum state { sBegin, sSingleQuote, sDoubleQuote };

    state st = sBegin;
    auto it = begin;
    for (; it != s.cend(); ++it) {
        if (st == sBegin) {
            std::cmatch match;
            if (regex_search(it, s.cend(), match, whitespace)) {
                cur.append(begin, it);
                res.push_back(cur);
                it = match[0].second;
                if (it == s.cend())
                    return res;
                begin = it;
                cur.clear();
            }
        }
        switch (*it) {
        case '\'':
            if (st != sDoubleQuote) {
                cur.append(begin, it);
                begin = it + 1;
                st = st == sBegin ? sSingleQuote : sBegin;
            }
            break;
        case '"':
            if (st != sSingleQuote) {
                cur.append(begin, it);
                begin = it + 1;
                st = st == sBegin ? sDoubleQuote : sBegin;
            }
            break;
        case '\\':
            if (st != sSingleQuote) {
                /* perl shellwords mostly just treats the next char as part of the string with no special processing */
                cur.append(begin, it);
                begin = ++it;
            }
            break;
        }
    }
    if (st != sBegin)
        throw Error("unterminated quote in shebang line");
    cur.append(begin, it);
    res.push_back(cur);
    return res;
}

/**
 * Like `resolveExprPath`, but prefers `shell.nix` instead of `default.nix`,
 * and if `path` was a directory, it checks eagerly whether `shell.nix` or
 * `default.nix` exist, throwing an error if they don't.
 */
static SourcePath resolveShellExprPath(SourcePath path)
{
    auto resolvedOrDir = resolveExprPath(path, false);
    if (resolvedOrDir.resolveSymlinks().lstat().type == SourceAccessor::tDirectory) {
        if ((resolvedOrDir / "shell.nix").pathExists()) {
            if (compatibilitySettings.nixShellAlwaysLooksForShellNix) {
                return resolvedOrDir / "shell.nix";
            } else {
                warn(
                    "Skipping '%1%', because the setting '%2%' is disabled. This is a deprecated behavior. Consider enabling '%2%'.",
                    resolvedOrDir / "shell.nix",
                    "nix-shell-always-looks-for-shell-nix");
            }
        }
        if ((resolvedOrDir / "default.nix").pathExists()) {
            return resolvedOrDir / "default.nix";
        }
        throw Error("neither '%s' nor '%s' found in '%s'", "shell.nix", "default.nix", resolvedOrDir);
    }
    return resolvedOrDir;
}

static void main_nix_build(int argc, char ** argv)
{
    auto dryRun = false;
    auto isNixShell = std::regex_search(argv[0], std::regex("nix-shell$"));
    auto pure = false;
    auto fromArgs = false;
    auto packages = false;
    // Same condition as bash uses for interactive shells
    auto interactive = isatty(STDIN_FILENO) && isatty(STDERR_FILENO);
    Strings attrPaths;
    Strings remainingArgs;
    BuildMode buildMode = bmNormal;
    bool readStdin = false;

    std::string envCommand; // interactive shell
    Strings envExclude;

    auto myName = isNixShell ? "nix-shell" : "nix-build";

    auto inShebang = false;
    std::filesystem::path script;
    std::vector<std::string> savedArgs;

    AutoDelete tmpDir(createTempDir("", myName));

    std::string outLink = "./result";

    // List of environment variables kept for --pure
    StringSet keepVars{
        "HOME",
        "XDG_RUNTIME_DIR",
        "USER",
        "LOGNAME",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "WAYLAND_SOCKET",
        "PATH",
        "TERM",
        "IN_NIX_SHELL",
        "NIX_SHELL_PRESERVE_PROMPT",
        "TZ",
        "PAGER",
        "NIX_BUILD_SHELL",
        "SHLVL",
    };
    keepVars.insert(networkProxyVariables.begin(), networkProxyVariables.end());

    Strings args;
    for (int i = 1; i < argc; ++i)
        args.push_back(argv[i]);

    // Heuristic to see if we're invoked as a shebang script, namely,
    // if we have at least one argument, it's the name of an
    // executable file, and it starts with "#!".
    if (isNixShell && argc > 1) {
        script = argv[1];
        try {
            auto lines = tokenizeString<Strings>(readFile(script), "\n");
            if (!lines.empty() && std::regex_search(lines.front(), std::regex("^#!"))) {
                lines.pop_front();
                inShebang = true;
                for (int i = 2; i < argc; ++i)
                    savedArgs.push_back(argv[i]);
                args.clear();
                for (auto line : lines) {
                    line = chomp(line);
                    std::smatch match;
                    if (std::regex_match(line, match, std::regex("^#!\\s*nix-shell\\s+(.*)$")))
                        for (const auto & word : shellwords({match[1].first, match[1].second}))
                            args.push_back(word);
                }
            }
        } catch (SystemError &) {
        }
    }

    struct MyArgs : LegacyArgs, MixEvalArgs
    {
        using LegacyArgs::LegacyArgs;

        void setBaseDir(std::filesystem::path baseDir)
        {
            commandBaseDir = baseDir.string();
        }
    };

    MyArgs myArgs(myName, [&](Strings::iterator & arg, const Strings::iterator & end) {
        if (*arg == "--help") {
            deletePath(tmpDir);
            showManPage(myName);
        }

        else if (*arg == "--version")
            printVersion(myName);

        else if (*arg == "--add-drv-link" || *arg == "--indirect")
            ; // obsolete

        else if (*arg == "--no-out-link" || *arg == "--no-link")
            outLink = (tmpDir.path() / "result").string();

        else if (*arg == "--attr" || *arg == "-A")
            attrPaths.push_back(getArg(*arg, arg, end));

        else if (*arg == "--drv-link")
            getArg(*arg, arg, end); // obsolete

        else if (*arg == "--out-link" || *arg == "-o")
            outLink = getArg(*arg, arg, end);

        else if (*arg == "--dry-run")
            dryRun = true;

        else if (*arg == "--run-env") // obsolete
            isNixShell = true;

        else if (isNixShell && (*arg == "--command" || *arg == "--run")) {
            if (*arg == "--run")
                interactive = false;
            envCommand = getArg(*arg, arg, end) + "\nexit";
        }

        else if (*arg == "--check")
            buildMode = bmCheck;

        else if (*arg == "--exclude")
            envExclude.push_back(getArg(*arg, arg, end));

        else if (*arg == "--expr" || *arg == "-E")
            fromArgs = true;

        else if (*arg == "--pure")
            pure = true;
        else if (*arg == "--impure")
            pure = false;

        else if (isNixShell && (*arg == "--packages" || *arg == "-p"))
            packages = true;

        else if (inShebang && *arg == "-i") {
            auto interpreter = getArg(*arg, arg, end);
            interactive = false;
            auto execArgs = "";

            // Überhack to support Perl. Perl examines the shebang and
            // executes it unless it contains the string "perl" or "indir",
            // or (undocumented) argv[0] does not contain "perl". Exploit
            // the latter by doing "exec -a".
            if (std::regex_search(interpreter, std::regex("perl")))
                execArgs = "-a PERL";

            std::ostringstream joined;
            for (const auto & i : savedArgs)
                joined << escapeShellArgAlways(i) << ' ';

            if (std::regex_search(interpreter, std::regex("ruby"))) {
                // Hack for Ruby. Ruby also examines the shebang. It tries to
                // read the shebang to understand which packages to read from. Since
                // this is handled via nix-shell -p, we wrap our ruby script execution
                // in ruby -e 'load' which ignores the shebangs.
                envCommand =
                    fmt("exec %1% %2% -e 'load(ARGV.shift)' -- %3% %4%",
                        execArgs,
                        interpreter,
                        escapeShellArgAlways(script.string()),
                        joined.view());
            } else {
                envCommand =
                    fmt("exec %1% %2% %3% %4%",
                        execArgs,
                        interpreter,
                        escapeShellArgAlways(script.string()),
                        joined.view());
            }
        }

        else if (*arg == "--keep")
            keepVars.insert(getArg(*arg, arg, end));

        else if (*arg == "-")
            readStdin = true;

        else if (*arg != "" && arg->at(0) == '-')
            return false;

        else
            remainingArgs.push_back(*arg);

        return true;
    });

    myArgs.parseCmdline(args);

    if (packages && fromArgs)
        throw UsageError("'-p' and '-E' are mutually exclusive");

    auto store = openStore();
    auto evalStore = myArgs.evalStoreUrl ? openStore(StoreReference{*myArgs.evalStoreUrl}) : store;

    auto state = std::make_shared<EvalState>(myArgs.lookupPath, evalStore, fetchSettings, evalSettings, store);
    state->repair = myArgs.repair;
    if (myArgs.repair)
        buildMode = bmRepair;

    if (inShebang && compatibilitySettings.nixShellShebangArgumentsRelativeToScript) {
        myArgs.setBaseDir(absPath(script.parent_path()));
    }
    if (packages) {
        std::ostringstream joined;
        joined
            << "{...}@args: with import <nixpkgs> args; (pkgs.runCommandCC or pkgs.runCommand) \"shell\" { buildInputs = [ ";
        for (const auto & i : remainingArgs)
            joined << '(' << i << ") ";
        joined << "]; } \"\"";
        fromArgs = true;
        remainingArgs = {joined.str()};
    } else if (!fromArgs && remainingArgs.empty()) {
        if (isNixShell && !compatibilitySettings.nixShellAlwaysLooksForShellNix
            && std::filesystem::exists("shell.nix")) {
            // If we're in 2.3 compatibility mode, we need to look for shell.nix
            // now, because it won't be done later.
            remainingArgs = {"shell.nix"};
        } else {
            remainingArgs = {"."};

            // Instead of letting it throw later, we throw here to give a more relevant error message
            if (isNixShell && !std::filesystem::exists("shell.nix") && !std::filesystem::exists("default.nix"))
                throw Error(
                    "no argument specified and no '%s' or '%s' file found in the working directory",
                    "shell.nix",
                    "default.nix");
        }
    }

    if (isNixShell)
        setEnv("IN_NIX_SHELL", pure ? "pure" : "impure");

    std::vector<RustBuiltDerivation> rustWanted;
    {
        if (isNixShell)
            refuse(refusalTokens::unsupported, "nix-shell");
        if (readStdin)
            refuse(refusalTokens::stdinSource, "reading the expression from stdin");
        if (inShebang)
            refuse(refusalTokens::unsupported, "nix-build in a #! script");
        if (attrPaths.empty())
            attrPaths = {""};
        auto rustAutoArgs = rustAutoArgsOf(myArgs);
        for (auto & i : remainingArgs) {
            RustSource src;
            if (fromArgs)
                src = RustSource{.source = i, .baseDir = absPath(".").string(), .file = ""};
            else {
                auto absolute = i;
                try {
                    absolute = canonPath(absPath(std::filesystem::path{i}), true).string();
                } catch (Error & e) {
                };
                auto [path, outputNames] = parsePathWithOutputs(absolute);
                if (evalStore->isStorePath(path) && hasSuffix(path, ".drv")) {
                    auto drvPath = evalStore->parseStorePath(path);
                    if (outputNames.empty())
                        outputNames.insert("out");
                    for (const auto & outputName : outputNames)
                        rustWanted.push_back(RustBuiltDerivation{.drvPath = drvPath, .outputName = outputName});
                    continue;
                }
                if (i.starts_with('<') || i.starts_with("flake:"))
                    refuse(refusalTokens::file, "'%s' (only a plain path)", i);
                auto resolved = resolveExprPath(lookupFileArg(*state, i)).path.abs();
                src = RustSource{
                    .source = readFile(resolved),
                    .baseDir = std::filesystem::path(resolved).parent_path().string(),
                    .file = resolved};
            }
            /* One evaluation of the root per expression, every attribute
               path walked from it -- cppnix's loop below, which evaluates
               `vRoot` once and selects each `-A` from it, so a trace at the
               root prints once however many paths are asked for. */
            RustEvaluand evaluand{.src = src, .args = {}, .attrPaths = attrPaths, .autoArgs = rustAutoArgs};
            for (auto & found : rustEvalDerivationSet(*state, evaluand))
                rustWanted.push_back(std::move(found));
        }
    }

    state->maybePrintStats();

    auto buildPaths = [&](const std::vector<DerivedPath> & paths) {
        if (settings.printMissing)
            printMissing(ref<Store>(store), paths);

        if (!dryRun)
            store->buildPaths(paths, buildMode, evalStore);
    };

    {
        std::vector<DerivedPath> pathsToBuild;
        std::vector<std::pair<StorePath, std::string>> pathsToBuildOrdered;
        RealisedPath::Set drvsToCopy;

        std::map<StorePath, std::pair<size_t, StringSet>> drvMap;

        auto wanted = std::move(rustWanted);

        for (auto & [drvPath, outputName] : wanted) {
            pathsToBuild.push_back(
                DerivedPath::Built{
                    .drvPath = makeConstantStorePathRef(drvPath),
                    .outputs = OutputsSpec::Names{outputName},
                });
            pathsToBuildOrdered.push_back({drvPath, {outputName}});
            drvsToCopy.insert(drvPath);

            auto i = drvMap.find(drvPath);
            if (i != drvMap.end())
                i->second.second.insert(outputName);
            else
                drvMap[drvPath] = {drvMap.size(), {outputName}};
        }

        buildPaths(pathsToBuild);

        if (dryRun)
            return;

        std::vector<StorePath> outPaths;

        for (auto & [drvPath, outputName] : pathsToBuildOrdered) {
            auto & [counter, _wantedOutputs] = drvMap.at({drvPath});
            std::string drvPrefix = outLink;
            if (counter)
                drvPrefix += fmt("-%d", counter + 1);

            auto builtOutputs = store->queryPartialDerivationOutputMap(drvPath, &*evalStore);

            auto maybeOutputPath = builtOutputs.at(outputName);
            assert(maybeOutputPath);
            auto outputPath = *maybeOutputPath;

            if (auto store2 = store.dynamic_pointer_cast<LocalFSStore>()) {
                std::string symlink = drvPrefix;
                if (outputName != "out")
                    symlink += "-" + outputName;
                store2->addPermRoot(outputPath, absPath(symlink));
            }

            outPaths.push_back(outputPath);
        }

        logger->stop();

        for (auto & path : outPaths)
            std::cout << store->printStorePath(path) << '\n';
    }
}

static RegisterLegacyCommand r_nix_build("nix-build", main_nix_build);
static RegisterLegacyCommand r_nix_shell("nix-shell", main_nix_build);
