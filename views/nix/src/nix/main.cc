#include "nix/cmd/common-eval-args.hh"
#include "nix/fetchers/fetch-settings.hh"
#include "nix/util/args/root.hh"
#include "nix/util/current-process.hh"
#include "nix/cmd/command.hh"
#include "nix/main/common-args.hh"
#include "nix/expr/eval.hh"
#include "nix/expr/eval-settings.hh"
#include "nix/store/globals.hh"
#include "nix/cmd/legacy.hh"
#include "nix/main/shared.hh"
#include "nix/store/store-open.hh"
#include "nix/store/store-registration.hh"
#include "nix/store/filetransfer.hh"
#include "nix/util/finally.hh"
#include "nix/main/loggers.hh"
#include "nix/cmd/markdown.hh"
#include "nix/util/memory-source-accessor.hh"
#include "nix/util/terminal.hh"
#include "nix/util/users.hh"
#include "nix/cmd/network-proxy.hh"
#include "nix/expr/eval-cache.hh"
#include "nix/expr/refusal-census.hh"
#include "nix/flake/flake.hh"
#include "nix/flake/settings.hh"
#include "nix/util/json-utils.hh"
#include "nix/expr/attr-path.hh"

#include "self-exe.hh"
#include "nix/cmd/rust-eval-session.hh"
#include "crash-handler.hh"
#include "invocation-record.hh"
#include "cli-config-private.hh"

#include <sys/types.h>
#include <nlohmann/json.hpp>

#ifndef _WIN32
#  include <sys/socket.h>
#  include <ifaddrs.h>
#  include <netdb.h>
#  include <netinet/in.h>
#endif

#ifdef __linux__
#  include "nix/util/linux-namespaces.hh"
#endif

#ifndef _WIN32
extern std::string chrootHelperName;

void chrootHelper(int argc, char ** argv);
#endif

#include "nix/util/strings.hh"

namespace nix {

/* Check if we have a non-loopback/link-local network interface. */
static bool haveInternet()
{
#ifndef _WIN32
    struct ifaddrs * addrs;

    if (getifaddrs(&addrs))
        return true;

    Finally free([&]() { freeifaddrs(addrs); });

    for (auto i = addrs; i; i = i->ifa_next) {
        if (!i->ifa_addr)
            continue;
        if (i->ifa_addr->sa_family == AF_INET) {
            if (ntohl(((sockaddr_in *) i->ifa_addr)->sin_addr.s_addr) != INADDR_LOOPBACK) {
                return true;
            }
        } else if (i->ifa_addr->sa_family == AF_INET6) {
            if (!IN6_IS_ADDR_LOOPBACK(&((sockaddr_in6 *) i->ifa_addr)->sin6_addr)
                && !IN6_IS_ADDR_LINKLOCAL(&((sockaddr_in6 *) i->ifa_addr)->sin6_addr))
                return true;
        }
    }

    if (haveNetworkProxyConnection())
        return true;

    return false;
#else
    // TODO implement on Windows
    return true;
#endif
}

std::string programPath;

struct NixArgs : virtual MultiCommand, virtual MixCommonArgs, virtual RootArgs
{
    bool useNet = true;
    bool refresh = false;
    bool helpRequested = false;
    bool showVersion = false;

    NixArgs()
        : MultiCommand("", RegisterCommand::getCommandsFor({}))
        , MixCommonArgs("nix")
    {
        categories.clear();
        categories[catHelp] = "Help commands";
        categories[Command::catDefault] = "Main commands";
        categories[catSecondary] = "Infrequently used commands";
        categories[catUtility] = "Utility/scripting commands";
        categories[catNixInstallation] = "Commands for upgrading or troubleshooting your Nix installation";

        addFlag({
            .longName = "help",
            .description = "Show usage information.",
            .category = miscCategory,
            .handler = {[this]() { this->helpRequested = true; }},
        });

        addFlag({
            .longName = "print-build-logs",
            .shortName = 'L',
            .description = "Print full build logs on standard error.",
            .category = loggingCategory,
            .handler = {[&]() { logger->setPrintBuildLogs(true); }},
            .experimentalFeature = Xp::NixCommand,
        });

        addFlag({
            .longName = "version",
            .description = "Show version information.",
            .category = miscCategory,
            .handler = {[&]() { showVersion = true; }},
        });

        addFlag({
            .longName = "offline",
            .aliases = {"no-net"}, // FIXME: remove
            .description = "Disable substituters and consider all previously downloaded files up-to-date.",
            .category = miscCategory,
            .handler = {[&]() { useNet = false; }},
            .experimentalFeature = Xp::NixCommand,
        });

        addFlag({
            .longName = "refresh",
            .description = "Consider all previously downloaded files out-of-date.",
            .category = miscCategory,
            .handler = {[&]() { refresh = true; }},
            .experimentalFeature = Xp::NixCommand,
        });

        aliases = {
            {"add-to-store", {AliasStatus::Deprecated, {"store", "add-path"}}},
            {"cat-nar", {AliasStatus::Deprecated, {"nar", "cat"}}},
            {"cat-store", {AliasStatus::Deprecated, {"store", "cat"}}},
            {"copy-sigs", {AliasStatus::Deprecated, {"store", "copy-sigs"}}},
            {"dev-shell", {AliasStatus::Deprecated, {"develop"}}},
            {"diff-closures", {AliasStatus::Deprecated, {"store", "diff-closures"}}},
            {"dump-path", {AliasStatus::Deprecated, {"store", "dump-path"}}},
            {"hash-file", {AliasStatus::Deprecated, {"hash", "file"}}},
            {"hash-path", {AliasStatus::Deprecated, {"hash", "path"}}},
            {"ls-nar", {AliasStatus::Deprecated, {"nar", "ls"}}},
            {"ls-store", {AliasStatus::Deprecated, {"store", "ls"}}},
            {"make-content-addressable", {AliasStatus::Deprecated, {"store", "make-content-addressed"}}},
            {"optimise-store", {AliasStatus::Deprecated, {"store", "optimise"}}},
            {"ping-store", {AliasStatus::Deprecated, {"store", "info"}}},
            {"sign-paths", {AliasStatus::Deprecated, {"store", "sign"}}},
            {"shell", {AliasStatus::AcceptedShorthand, {"env", "shell"}}},
            {"show-derivation", {AliasStatus::Deprecated, {"derivation", "show"}}},
            {"show-config", {AliasStatus::Deprecated, {"config", "show"}}},
            {"to-base16", {AliasStatus::Deprecated, {"hash", "to-base16"}}},
            {"to-base32", {AliasStatus::Deprecated, {"hash", "to-base32"}}},
            {"to-base64", {AliasStatus::Deprecated, {"hash", "to-base64"}}},
            {"verify", {AliasStatus::Deprecated, {"store", "verify"}}},
            {"doctor", {AliasStatus::Deprecated, {"config", "check"}}},
        };
    };

    std::string description() override
    {
        return "a tool for reproducible and declarative configuration management";
    }

    std::string doc() override
    {
        return
#include "nix.md"
            ;
    }

    // Plugins may add new subcommands.
    void pluginsInited() override
    {
        commands = RegisterCommand::getCommandsFor({});
    }

    std::string dumpCli()
    {
        using nlohmann::json;

        auto res = json::object();

        res["args"] = toJSON();

        {
            auto & stores = res["stores"] = json::object();
            for (auto & [storeName, implem] : Implementations::registered()) {
                auto & j = stores[storeName];
                j["doc"] = implem.doc;
                j["uri-schemes"] = implem.uriSchemes;
                j["settings"] = implem.getConfig()->toJSON();
                j["experimentalFeature"] = implem.experimentalFeature;
            }
        }

        {
            auto & fetchers = res["fetchers"] = json::object();

            for (const auto & [schemeName, scheme] : fetchers::getAllInputSchemes()) {
                auto & s = fetchers[schemeName] = json::object();
                s["description"] = scheme->schemeDescription();
                auto & attrs = s["allowedAttrs"] = json::object();
                for (auto & [fieldName, field] : scheme->allowedAttrs()) {
                    auto & f = attrs[fieldName] = json::object();
                    f["type"] = field.type;
                    f["required"] = field.required;
                    f["doc"] = stripIndentation(field.doc);
                }
            }
        };

        return res.dump();
    }
};

/* Render the help for the specified subcommand to stdout using
   lowdown. */
static void showHelp(std::vector<std::string> subcommand, NixArgs & toplevel)
{
    // Check for aliases if subcommand has exactly one element
    if (subcommand.size() == 1) {
        auto alias = toplevel.aliases.find(subcommand[0]);
        if (alias != toplevel.aliases.end()) {
            subcommand = alias->second.replacement;
        }
    }

    auto mdName = subcommand.empty() ? "nix" : fmt("nix3-%s", concatStringsSep("-", subcommand));

    evalSettings.restrictEval = true;
    evalSettings.pureEval = true;
    auto statePtr = std::make_shared<EvalState>(
        LookupPath{},
        openStore(StoreReference{.variant = StoreReference::Specified{.scheme = "dummy"}}),
        fetchSettings,
        evalSettings);
    auto & state = *statePtr;

    state.corepkgsFS->addFile(
        CanonPath("utils.nix"),
#include "utils.nix.gen.hh"
    );

    state.corepkgsFS->addFile(
        CanonPath("/generate-settings.nix"),
#include "generate-settings.nix.gen.hh"
    );

    state.corepkgsFS->addFile(
        CanonPath("/generate-store-info.nix"),
#include "generate-store-info.nix.gen.hh"
    );

    // Render help through the same evaluator as user expressions.
    std::string markdown;
    {
        RustEvaluand evaluand{
            .src =
                RustSource{
                    .source =
#include "generate-manpage.nix.gen.hh"
                        , .baseDir = "/", .file = "",
                },
            .args =
                {RustArgument{.kind = RustArgument::Kind::Json, .text = "false"},
                 RustArgument{.kind = RustArgument::Kind::Json, .text = nlohmann::json(toplevel.dumpCli()).dump()}},
            .attrPaths = {fmt("\"%s.md\"", mdName)},
        };
        try {
            markdown = rustEvalRender(state, evaluand, RustRender::Raw);
        } catch (AttrPathNotFound &) {
            throw UsageError("Nix has no subcommand '%s'", concatStringsSep(" ", subcommand));
        }
    }

    /* `NIX_SHOW_STATS` names the evaluator that answered (`evaluator`,
       `evaluatorCalls`), the same witness every routed command prints; the
       functional test reads it to prove the page was Rust's and not a
       fallback. */
    state.maybePrintStats();

    RunPager pager;
    std::cout << renderMarkdownToTerminal(markdown) << "\n";
}

static NixArgs & getNixArgs(Command & cmd)
{
    return dynamic_cast<NixArgs &>(cmd.getRoot());
}

struct CmdHelp : Command
{
    std::vector<std::string> subcommand;

    CmdHelp()
    {
        expectArgs({
            .label = "subcommand",
            .handler = {&subcommand},
        });
    }

    std::string description() override
    {
        return "show help about `nix` or a particular subcommand";
    }

    std::string doc() override
    {
        return
#include "help.md"
            ;
    }

    Category category() override
    {
        return catHelp;
    }

    void run() override
    {
        assert(parent);
        MultiCommand * toplevel = parent;
        while (toplevel->parent)
            toplevel = toplevel->parent;
        showHelp(subcommand, getNixArgs(*this));
    }
};

static auto rCmdHelp = registerCommand<CmdHelp>("help");

struct CmdHelpStores : Command
{
    std::string description() override
    {
        return "show help about store types and their settings";
    }

    std::string doc() override
    {
        return
#include "help-stores.md.gen.hh"
            ;
    }

    Category category() override
    {
        return catHelp;
    }

    void run() override
    {
        showHelp({"help-stores"}, getNixArgs(*this));
    }
};

static auto rCmdHelpStores = registerCommand<CmdHelpStores>("help-stores");

/**
 * The subcommand path the user typed, e.g. `{"flake", "metadata"}`.
 *
 * A `MultiCommand` whose selected command is itself a `MultiCommand` nests, so
 * a walk that stops at the first level calls every `nix flake *` invocation
 * `nix flake`. Written once because two readers need the same answer: `--help`
 * to find the page, and the refusal census to name the command a refusal came
 * from.
 */
static std::vector<std::string> subcommandPath(MultiCommand & root)
{
    std::vector<std::string> path;
    MultiCommand * command = &root;
    while (command && command->command) {
        path.push_back(command->command->first);
        command = dynamic_cast<MultiCommand *>(&*command->command->second);
    }
    return path;
}

void mainWrapped(int argc, char ** argv)
{
    savedArgv = argv;

    registerCrashHandler();

    /* The chroot helper needs to be run before any threads have been
       started. */
#ifndef _WIN32
    if (argc > 0 && argv[0] == chrootHelperName) {
        chrootHelper(argc, argv);
        return;
    }
#endif

    /* Set the build hook location

       For builds we perform a self-invocation, so Nix has to be
       self-aware. That is, it has to know where it is installed. We
       don't think it's sentient.
     */
    settings.getWorkerSettings().buildHook.setDefault(
        Strings{
            getNixBin({}).string(),
            "__build-remote",
        });

    initNix();
    initGC();
    if (!std::string_view(NIX_HOST_BUILD_IDENTITY).empty())
        rustSetHostBuildIdentity(NIX_HOST_BUILD_IDENTITY);

#ifdef __linux__
    if (isRootUser()) {
        try {
            saveMountNamespace();
            if (unshare(CLONE_NEWNS) == -1)
                throw SysError("setting up a private mount namespace");
        } catch (Error & e) {
        }
    }
#endif

    programPath = argv[0];
    auto programName = std::string(baseNameOf(programPath));
    auto extensionPos = programName.find_last_of(".");
    if (extensionPos != std::string::npos)
        programName.erase(extensionPos);

    if (argc > 1 && std::string_view(argv[1]) == "__build-remote") {
        programName = "build-remote";
        argv++;
        argc--;
    }

    {
        if (auto legacy = get(RegisterLegacyCommand::commands(), programName))
            return (*legacy)(argc, argv);
    }

    evalSettings.pureEval = true;

#ifndef _WIN32
    setLogFormat("bar");
#endif
    settings.verboseBuild = false;

    // If on a terminal, progress will be displayed via progress bars etc. (thus verbosity=notice)
    if (nix::isTTY()) {
        verbosity = lvlNotice;
    } else {
        verbosity = lvlInfo;
    }

    NixArgs args;

    if (argc == 2 && std::string(argv[1]) == "__dump-cli") {
        logger->cout(args.dumpCli());
        return;
    }

    if (argc == 2 && std::string(argv[1]) == "__dump-language") {
        logger->cout("%s", rustLanguageDocs());
        return;
    }

    if (argc == 2 && std::string(argv[1]) == "__dump-xp-features") {
        logger->cout(documentExperimentalFeatures().dump());
        return;
    }

    Finally printCompletions([&]() {
        if (args.completions) {
            switch (args.completions->type) {
            case Completions::Type::Normal:
                logger->cout("normal");
                break;
            case Completions::Type::Filenames:
                logger->cout("filenames");
                break;
            case Completions::Type::Attrs:
                logger->cout("attrs");
                break;
            }
            for (auto & s : args.completions->completions)
                logger->cout(s.completion + "\t" + trim(s.description));
        }
    });

    try {
        auto isNixCommand = programName.ends_with("nix");
        auto allowShebang = isNixCommand && argc > 1;
        args.parseCmdline(argvToStrings(argc, argv), allowShebang);
    } catch (UsageError &) {
        if (!args.helpRequested && !args.completions)
            throw;
    }

    applyJSONLogger();

    /* Name this command for the refusal census, before anything can evaluate
       and therefore before anything can refuse. `argv[0]` alone would file
       every `nix ...` refusal under one row, which is the row that has to be
       split for the histogram to say which command to wire up next.

       Ahead of the help branch rather than after it, because `showHelp` builds
       its page by evaluating `generate-manpage.nix` through the selected
       backend, so anything the Rust arm cannot evaluate in the generator is
       filed under `nix flake metadata --help` rather than under bare `nix`. */
    {
        auto name = programName;
        for (const auto & component : subcommandPath(args))
            name += " " + component;
        RefusingCommand::set(name);
    }

    if (args.helpRequested) {
        showHelp(subcommandPath(args), args);
        return;
    }

    if (args.completions)
        return;

    if (args.showVersion) {
        printVersion(programName);
        return;
    }

    if (!args.command)
        throw UsageError("no subcommand specified");

    experimentalFeatureSettings.require(args.command->second->experimentalFeature());

    /* Reading a record is not itself worth recording. */
    invocationRecord::start(argvToStrings(argc, argv), args.command->first == "invocation");

    if (args.useNet && !haveInternet()) {
        warn("you don't have Internet access; disabling some network-dependent features");
        args.useNet = false;
    }

    if (!args.useNet) {
        // FIXME: should check for command line overrides only.
        if (!settings.getWorkerSettings().useSubstitutes.overridden)
            settings.getWorkerSettings().useSubstitutes = false;
        if (!fetchSettings.tarballTtl.overridden)
            fetchSettings.tarballTtl = std::numeric_limits<unsigned int>::max();
        if (!fileTransferSettings.tries.overridden)
            fileTransferSettings.tries = 0;
        if (!fileTransferSettings.connectTimeout.overridden)
            fileTransferSettings.connectTimeout = 1;
        auto & ttlMeta = settings.getNarInfoDiskCacheSettings().ttlMeta;
        if (!ttlMeta.overridden)
            ttlMeta = std::numeric_limits<unsigned int>::max();
    }

    if (args.refresh) {
        fetchSettings.tarballTtl = 0;
        settings.getNarInfoDiskCacheSettings().ttlNegative = 0;
        settings.getNarInfoDiskCacheSettings().ttlPositive = 0;
        settings.getNarInfoDiskCacheSettings().ttlMeta = 0;
    }

    if (args.command->second->forceImpureByDefault() && !evalSettings.pureEval.overridden) {
        evalSettings.pureEval = false;
    }

    try {
        args.command->second->run();
    } catch (eval_cache::CachedEvalError & e) {
        /* Evaluate the original attribute that resulted in this
           cached error so that we can show the original error to the
           user. */
        e.force();
    }
}

} // namespace nix

int main(int argc, char ** argv)
{
    // The CLI has a more detailed version than the libraries; see nixVersion.
    nix::nixVersion = NIX_CLI_VERSION;
#ifndef _WIN32
    // Increase the default stack size for the evaluator and for
    // libstdc++'s std::regex.
    // This used to be 64 MiB, but macOS as deployed on GitHub Actions has a
    // hard limit slightly under that, so we round it down a bit.
    nix::setStackSize(nix::evalStackSize);
#endif

    auto status = nix::handleExceptions(argv[0], [&]() { nix::mainWrapped(argc, argv); });
    /* After `mainWrapped` has returned, so `~EvalCommand` has already written
       the evaluator statistics into the record, and after the status is
       known. */
    nix::invocationRecord::finish(status);
    return status;
}
