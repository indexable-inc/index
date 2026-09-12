#include "nix/store/globals.hh"
#include "nix/expr/print-ambiguous.hh"
#include "nix/main/shared.hh"
#include "nix/expr/eval.hh"
#include "nix/expr/eval-inline.hh"
#include "nix/expr/get-drvs.hh"
#include "nix/expr/attr-path.hh"
#include "nix/util/signals.hh"
#include "nix/expr/value-to-xml.hh"
#include "nix/expr/value-to-json.hh"
#include "nix/store/store-open.hh"
#include "nix/store/local-fs-store.hh"
#include "nix/cmd/common-eval-args.hh"
#include "nix/cmd/legacy.hh"
#include "man-pages.hh"
#include "rust-eval.hh"
#include "nix/expr/rust-eval-refusal.hh"
#include "nix/cmd/rust-eval-session.hh"

#include <map>
#include <iostream>
#include <sstream>
#include <exception>
#include <optional>

using namespace nix;

std::filesystem::path gcRoot;
static int rootNr = 0;

enum OutputKind { okPlain, okRaw, okXML, okJSON };

static int main_nix_instantiate(int argc, char ** argv)
{
    {
        Strings files;
        bool readStdin = false;
        bool fromArgs = false;
        bool findFile = false;
        bool evalOnly = false;
        bool parseOnly = false;
        OutputKind outputKind = okPlain;
        bool xmlOutputSourceLocation = true;
        bool strict = false;
        Strings attrPaths;
        bool wantsReadWrite = false;

        struct MyArgs : LegacyArgs, MixEvalArgs
        {
            using LegacyArgs::LegacyArgs;
        };

        MyArgs myArgs(std::string(baseNameOf(argv[0])), [&](Strings::iterator & arg, const Strings::iterator & end) {
            if (*arg == "--help")
                showManPage("nix-instantiate");
            else if (*arg == "--version")
                printVersion("nix-instantiate");
            else if (*arg == "-")
                readStdin = true;
            else if (*arg == "--expr" || *arg == "-E")
                fromArgs = true;
            else if (*arg == "--eval" || *arg == "--eval-only")
                evalOnly = true;
            else if (*arg == "--read-write-mode")
                wantsReadWrite = true;
            else if (*arg == "--parse" || *arg == "--parse-only")
                parseOnly = evalOnly = true;
            else if (*arg == "--find-file")
                findFile = true;
            else if (*arg == "--attr" || *arg == "-A")
                attrPaths.push_back(getArg(*arg, arg, end));
            else if (*arg == "--add-root")
                gcRoot = getArg(*arg, arg, end);
            else if (*arg == "--indirect")
                ;
            else if (*arg == "--raw")
                outputKind = okRaw;
            else if (*arg == "--xml")
                outputKind = okXML;
            else if (*arg == "--json")
                outputKind = okJSON;
            else if (*arg == "--no-location")
                xmlOutputSourceLocation = false;
            else if (*arg == "--strict")
                strict = true;
            else if (*arg == "--dry-run")
                settings.readOnlyMode = true;
            else if (*arg != "" && arg->at(0) == '-')
                return false;
            else
                files.push_back(*arg);
            return true;
        });

        myArgs.parseCmdline(argvToStrings(argc, argv));

        if (evalOnly && !wantsReadWrite)
            settings.readOnlyMode = true;

        auto store = openStore();
        auto evalStore = myArgs.evalStoreUrl ? openStore(StoreReference{*myArgs.evalStoreUrl}) : store;

        auto state = std::make_shared<EvalState>(myArgs.lookupPath, evalStore, fetchSettings, evalSettings, store);

        // On a scope guard rather than a straight-line call at the end, so the
        // stats survive a throw.
        //
        // A refusal is fatal: it throws, and every statement after it is
        // skipped. `maybePrintStats()` sat below the work, so the one run that
        // had a refusal to report was exactly the run that reported nothing --
        // the counters read empty precisely when they had something to say.
        // `nix eval` never had this problem because `EvalCommand::~EvalCommand`
        // runs during unwinding; this is nix-instantiate catching up.
        //
        // The journal line is still the production census, for the reason
        // argued on `RefusalCensus`: this only helps a process that gets to
        // unwind, and says nothing about one that is killed.
        struct PrintStatsOnTheWayOut
        {
            EvalState & state;

            ~PrintStatsOnTheWayOut()
            {
                try {
                    state.maybePrintStats();
                } catch (const std::exception & e) {
                    // Reporting must never replace the error being reported,
                    // and throwing from a destructor while unwinding calls
                    // std::terminate. Named on stderr rather than swallowed,
                    // because "the stats did not print" is itself something
                    // the reader needs to know.
                    std::cerr << "nix-instantiate: could not print stats: " << e.what() << std::endl;
                } catch (...) {
                    std::cerr << "nix-instantiate: could not print stats" << std::endl;
                }
            }
        } printStatsOnTheWayOut{*state};

        state->repair = myArgs.repair;

        if (attrPaths.empty())
            attrPaths = {""};

        if (findFile) {
            for (auto & i : files) {
                auto p = state->findFile(i);
                if (auto fn = p.getPhysicalPath())
                    std::cout << fn->string() << std::endl;
                else
                    throw Error("'%s' has no physical path", p);
            }
            return 0;
        }

        if (parseOnly)
            refuse(refusalTokens::unsupported, "nix-instantiate --parse");
        if (readStdin)
            refuse(refusalTokens::stdinSource, "reading from stdin");
        if (files.empty() && !fromArgs)
            files.push_back("./default.nix");

        auto autoArgs = rustAutoArgsOf(myArgs);
        for (auto & input : files) {
            RustSource source;
            if (fromArgs)
                source = RustSource{.source = input, .baseDir = absPath(".").string(), .file = ""};
            else {
                auto path = resolveExprPath(lookupFileArg(*state, input)).path.abs();
                source = RustSource{
                    .source = readFile(path),
                    .baseDir = std::filesystem::path(path).parent_path().string(),
                    .file = path};
            }
            if (evalOnly) {
                rustEvalPrint(
                    *state,
                    source.source,
                    source.baseDir,
                    source.file,
                    attrPaths,
                    outputKind,
                    xmlOutputSourceLocation,
                    strict,
                    autoArgs);
                continue;
            }
            RustEvaluand evaluand{.src = source, .args = {}, .attrPaths = attrPaths, .autoArgs = autoArgs};
            for (const auto & found : rustEvalDerivationSet(*state, evaluand)) {
                auto path = evalStore->printStorePath(found.drvPath);
                if (gcRoot.empty())
                    printGCWarning();
                else if (auto local = evalStore.dynamic_pointer_cast<LocalFSStore>()) {
                    auto root = absPath(gcRoot);
                    if (++rootNr > 1)
                        root += "-" + std::to_string(rootNr);
                    path = local->addPermRoot(found.drvPath, root).string();
                }
                std::cout << path;
                if (found.outputName != "out")
                    std::cout << "!" << found.outputName;
                std::cout << "\n";
            }
        }

        return 0;
    }
}

static RegisterLegacyCommand r_nix_instantiate("nix-instantiate", main_nix_instantiate);
