#include "ixe-flake-check.h"
#include "nix/cmd/rust-eval-session.hh"
#include "nix/cmd/rust-command.hh"
#include "nix/expr/get-drvs.hh"
#include "nix/fetchers/jj-tree.hh"
#include <functional>
#include "nix/expr/eval-perf-census.hh"
#include "nix/expr/rust-eval-refusal.hh"

#include "nix/util/error.hh"
#include "nix/util/environment-variables.hh"
#include "nix/util/users.hh" // getHome, for `~/...` path literals
#include "nix/util/util.hh"
#include "nix/expr/eval-error.hh"
#include "nix/expr/attr-path.hh"
#include "nix/store/derivations.hh"
#include "nix/store/downstream-placeholder.hh"
#include "nix/expr/print.hh"
#include "nix/store/globals.hh" // nixVersion
#include "nix/store/store-api.hh"
#include "nix/store/build-result.hh"
#include "nix/store/worker-protocol-impl.hh" // StorePathSet envelope reader
#include "nix/util/serialise.hh"             // sinkToSource
#include "nix/fetchers/fetch-to-store.hh"
#include "nix/fetchers/filtering-source-accessor.hh"
#include "nix/util/mounted-source-accessor.hh"
#include "nix/fetchers/tarball.hh"
#include "nix/fetchers/registry.hh"
#include "nix/fetchers/input-cache.hh"
#include "nix/flake/flake-document.hh"
#include "nix/flake/flake.hh"
#include "nix/flake/flakeref.hh"
#include "nix/flake/settings.hh"
#include "nix/flake/flakeref.hh"
#include "nix/flake/lockfile.hh"
#include "nix/cmd/installable-flake.hh"
#include "nix/cmd/installable-derived-path.hh"
#include "nix/store/outputs-spec.hh"
#include "nix/store/names.hh"
#include "nix/expr/value-to-json.hh"

#include <nlohmann/json.hpp>
#include "nix/util/finally.hh"
#include "nix/util/signals.hh" // isInterrupted
#include "nix/util/hash.hh"    // the divergence id

#include <algorithm> // std::sort, over the derivation lines
#include <iostream>
#include <sstream>
#include <chrono>
#include <cstring> // strnlen, over the names buffer
#include <filesystem>

#include "ixe.h"
#include "ixe-command.h"
#include "ixe-flake-show.h"
#include "ixe-search.h"
#include "nix/cmd/rust-search.hh"
#include "ixe-source-position.h"

namespace nix {

std::optional<RustSource> rustReadSource(SourceExprCommand & cmd)
{
    if (cmd.expr)
        return RustSource{*cmd.expr, absPath(cmd.getCommandBaseDir()).string(), ""};
    auto arg = cmd.file->string();
    // `<nixpkgs>` is a search-path lookup (ENG-12443) and a flake ref is a
    // fetch; `lookupFileArg` would quietly resolve either through the C++
    // evaluator, which is how a backend comes to be credited with an answer
    // it did not produce.
    if (arg.starts_with("<") || arg.find(':') != std::string::npos)
        return std::nullopt;
    auto dir = absPath(cmd.getCommandBaseDir());
    auto path = absPath(arg, &dir);
    if (std::filesystem::is_directory(path))
        path = path / "default.nix";
    return RustSource{readFile(path.string()), path.parent_path().string(), path.string()};
}

std::optional<RustSource> rustSourceOf(SourceExprCommand & cmd)
{
    if (cmd.file && cmd.expr)
        throw UsageError("'--file' and '--expr' are exclusive");
    // Neither given: the positional argument is an installable, and what it
    // means -- a store path, a flake reference -- cannot be settled without an
    // evaluator. `rustEvaluandOf` settles it one phase later. This used to
    // refuse here, which is where every flake invocation stopped.
    if (!cmd.file && !cmd.expr)
        return std::nullopt;
    if (cmd.file && *cmd.file == "-")
        refuse(refusalTokens::stdinSource, "reading the expression from stdin");

    // Configure file access before constructing the host's restricted accessor.
    if (cmd.file) {
        if (evalSettings.pureEval && evalSettings.pureEval.overridden)
            throw UsageError("'--file' is not compatible with '--pure-eval'");
        evalSettings.pureEval = false;
    }

    auto read = rustReadSource(cmd);
    if (!read)
        refuse(refusalTokens::file, "--file '%s' (only a plain path)", cmd.file->string());
    return *read;
}

std::vector<RustAutoArg> rustAutoArgsOf(const MixEvalArgs & args)
{
    /* Mirrors `MixEvalArgs::getAutoArgs`: an expression stays an expression
       (the evaluator parses it under the working directory, as cppnix
       parses it under `rootPath(".")`), and the three string shapes are the
       bytes cppnix would bind -- a file's contents read here, once, the way
       cppnix reads them there. In `std::map` order, which is name order, so
       the same command line keys the same. */
    std::vector<RustAutoArg> out;
    out.reserve(args.autoArgs.size());
    for (auto & [name, arg] : args.autoArgs) {
        std::visit(
            overloaded{
                [&](const MixEvalArgs::AutoArgExpr & expr) {
                    out.push_back(RustAutoArg{.kind = RustAutoArg::Kind::Expr, .name = name, .text = expr.expr});
                },
                [&](const MixEvalArgs::AutoArgString & str) {
                    out.push_back(RustAutoArg{.kind = RustAutoArg::Kind::String, .name = name, .text = str.s});
                },
                [&](const MixEvalArgs::AutoArgFile & file) {
                    out.push_back(
                        RustAutoArg{
                            .kind = RustAutoArg::Kind::String, .name = name, .text = readFile(file.path.string())});
                },
                [&](const MixEvalArgs::AutoArgStdin &) {
                    out.push_back(
                        RustAutoArg{.kind = RustAutoArg::Kind::String, .name = name, .text = readFile(STDIN_FILENO)});
                }},
            arg);
    }
    return out;
}

namespace {

struct FlakeShowError
{
    char * text = nullptr;

    ~FlakeShowError()
    {
        ixe_string_free(text);
    }

    void check(int status) const
    {
        if (status != 0)
            throw Error("rust-eval: %s", text ? text : "flake-show operation failed");
    }
};

IxeBytes flakeShowBytes(std::string_view value)
{
    return {reinterpret_cast<const unsigned char *>(value.data()), value.size()};
}

std::string flakeShowString(IxeBytes value)
{
    return std::string(reinterpret_cast<const char *>(value.text), value.len);
}
} // namespace

void RustFlakeShowDocument::Deleter::operator()(IxeFlakeShowDocument * document) const
{
    ixe_flake_show_free(document);
}

RustFlakeShowDocument::RustFlakeShowDocument(IxeFlakeShowDocument * document)
    : document(document)
{
}

RustFlakeShowDocument::RustFlakeShowDocument()
{
    FlakeShowError error;
    IxeFlakeShowDocument * result = nullptr;
    error.check(ixe_flake_show_new(&result, &error.text));
    document.reset(result);
}

uint64_t RustFlakeShowDocument::addNode(
    unsigned int kind,
    std::string_view name,
    const std::optional<std::string> & description,
    const std::map<std::string, uint64_t> & children)
{
    std::vector<IxeFlakeShowChild> borrowed;
    borrowed.reserve(children.size());
    for (auto & [childName, child] : children)
        borrowed.push_back({flakeShowBytes(childName), child});
    uint64_t result = 0;
    FlakeShowError error;
    error.check(ixe_flake_show_add(
        document.get(),
        kind,
        flakeShowBytes(name),
        description.has_value(),
        flakeShowBytes(description ? std::string_view(*description) : std::string_view()),
        borrowed.data(),
        borrowed.size(),
        &result,
        &error.text));
    return result;
}

void RustFlakeShowDocument::finish(uint64_t root)
{
    FlakeShowError error;
    error.check(ixe_flake_show_finish(document.get(), root, &error.text));
}

std::string RustFlakeShowDocument::encode() const
{
    FlakeShowError error;
    char * encoded = nullptr;
    error.check(ixe_flake_show_encode(document.get(), &encoded, &error.text));
    std::unique_ptr<char, decltype(&ixe_string_free)> owned(encoded, ixe_string_free);
    return std::string(owned.get());
}

RustFlakeShowDocument RustFlakeShowDocument::decode(const std::string & encoded)
{
    FlakeShowError error;
    IxeFlakeShowDocument * result = nullptr;
    error.check(ixe_flake_show_decode(
        reinterpret_cast<const unsigned char *>(encoded.data()), encoded.size(), &result, &error.text));
    return RustFlakeShowDocument(result);
}

RustFlakeShowRenderReport RustFlakeShowDocument::render(std::string_view rootLabel, bool json) const
{
    FlakeShowError error;
    IxeFlakeShowReport * result = nullptr;
    error.check(ixe_flake_show_render(document.get(), flakeShowBytes(rootLabel), json, true, &result, &error.text));
    std::unique_ptr<IxeFlakeShowReport, decltype(&ixe_flake_show_report_free)> owned(
        result, ixe_flake_show_report_free);
    IxeFlakeShowReportView view;
    error.check(ixe_flake_show_report_view(owned.get(), &view));
    RustFlakeShowRenderReport report{.output = flakeShowString(view.output), .warnings = {}};
    report.warnings.reserve(view.warnings_len);
    for (size_t index = 0; index < view.warnings_len; ++index)
        report.warnings.push_back(flakeShowString(view.warnings[index].message));
    return report;
}

/// Everything a host callback needs: the `EvalState` it answers out of, and
/// the buffers it answers into.
///
/// One of these per `RustEvalSetup`, reached through the `ctx` pointer the
/// vtable carries, so a callback's world is the session that installed it.
/// This used to be a `thread_local EvalState *` and fourteen `thread_local
/// std::string`s, because a hook was a bare C function pointer in a
/// process-global slot with nowhere to put a closure: two sessions in one
/// process shared one set of buffers and one notion of "the current state",
/// and the second one to start silently answered out of the first one's.
///
/// Each buffer is separate rather than one shared scratch string. The Rust
/// side copies an answer before returning, so a buffer only has to outlive
/// its own call -- but two hooks can be in flight over one path (`file_type`
/// and `file_type_resolved` ask different questions about it), and a shared
/// buffer would let one overwrite the other between the write and the copy.
struct RustEvalHost
{
    EvalState & state;

    uint64_t copyMounted = 0;
    uint64_t copyAmbient = 0;
    uint64_t drvWrites = 0;
    uint64_t drvFlushes = 0;

    /// The `readDir` hook, by count and by half: resolving the directory's
    /// path (symlinks, per component) and listing it (entries plus the
    /// `lstat` per entry whose type the accessor did not report). A cold
    /// walk of the hydra configuration makes 8,084 of these at ~310 us each
    /// (goals/rust-eval.md, 2026-09-04); which half that is decides whether
    /// the fix is a cheaper resolve or a listing that rides the tree object.
    uint64_t readDirCalls = 0;
    uint64_t readDirResolveNs = 0;
    uint64_t readDirListNs = 0;

    std::string copyToStoreAnswer;
    std::string storePathAnswer;
    std::string storeTextAnswer;
    std::string writeDrvAnswer;
    std::string validPathsAnswer;
    std::string sealedPathsAnswer;
    std::string allowPathsAnswer;
    std::string storeFilteredAnswer;
    std::string fetchAnswer;
    std::string fetchTreeAnswer;
    std::string lockFlakeAnswer;
    std::string parseFlakeRefAnswer;
    std::string flakeRefToStringAnswer;
    std::string importAnswer;
    std::string readFileAnswer;
    std::string pathExistsAnswer;
    std::string dirExistsAnswer;
    std::string readDirAnswer;
    std::string fileTypeAnswer;
    std::string fileTypeResolvedAnswer;
    std::string ensurePathAnswer;
    std::string realiseCheckAnswer;
    std::string realiseAllowAnswer;
    std::string findFileAnswer;
    std::string nixPathAnswer;

    /// The struct handed to the evaluator, pointing back at this object.
    IxeHostVtable vtable;

    explicit RustEvalHost(EvalState & state);
};

/// The host a callback's context pointer names.
///
/// Not a recoverable case and therefore not a check that returns an error:
/// `RustEvalSetup` sets `ctx` to the host it owns, `ixe_session_new` refuses
/// a null vtable, and the evaluator hands `ctx` back unchanged. A null here
/// would be the ABI misbehaving. Every callback below used to open with
/// `if (!currentState) return answer("no evaluator state", 1)`, which was a
/// real case when the state was a process-wide slot a caller could forget to
/// set; it cannot be one when the state arrives with the call.
static RustEvalHost & hostOf(void * ctx)
{
    assert(ctx);
    return *static_cast<RustEvalHost *>(ctx);
}

static SourcePath
rootedPath(RustEvalHost & host, const unsigned char * root, size_t rootLen, const unsigned char * path, size_t pathLen)
{
    std::string p(reinterpret_cast<const char *>(path), pathLen);
    auto relative = CanonPath(p);
    if (relative.abs() != p)
        throw Error("rust-eval: accessor path '%s' is not canonical", p);
    if (rootLen == 0)
        return host.state.rootPath(std::move(relative));

    std::string mountPoint(reinterpret_cast<const char *>(root), rootLen);
    auto canonicalMount = CanonPath(mountPoint);
    if (canonicalMount.abs() != mountPoint)
        throw Error("rust-eval: mounted path root '%s' is not canonical", mountPoint);
    auto [storePath, subPath] = host.state.store->toStorePath(mountPoint);
    if (!subPath.isRoot() || host.state.store->printStorePath(storePath) != mountPoint)
        throw Error("rust-eval: mounted path root '%s' is not a complete store path", mountPoint);
    auto accessor = host.state.storeFS->getMount(canonicalMount);
    if (!accessor)
        throw Error("rust-eval: mounted path root '%s' disappeared before host question for '%s'", mountPoint, p);
    return SourcePath{ref(accessor), std::move(relative)};
}

static int rustImport(
    void * ctx,
    const unsigned char * root,
    size_t rootLen,
    const unsigned char * path,
    size_t pathLen,
    const unsigned char ** out,
    size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & bytes, int rc) {
        host.importAnswer = bytes;
        *out = reinterpret_cast<const unsigned char *>(host.importAnswer.data());
        *outLen = host.importAnswer.size();
        return rc;
    };

    try {
        std::string rootName(reinterpret_cast<const char *>(root), rootLen);
        SourcePath source = rootedPath(host, root, rootLen, path, pathLen);

        // Match ordinary import before source/directory resolution. A .drv
        // suffix alone is not enough: the object must be a valid store root.
        auto & store = *host.state.store;
        auto pathText = source.path.abs();
        if (store.isStorePath(pathText) && isDerivation(pathText)) {
            auto drvPath = store.parseStorePath(pathText);
            if (store.isValidPath(drvPath)) {
                auto drv = store.readDerivation(drvPath);
                auto outputs = nlohmann::json::object();
                for (const auto & [name, output] : drv.outputs) {
                    auto staticPath = output.path(store, Derivation::nameFromPath(drvPath), name);
                    outputs[name] = staticPath ? store.printStorePath(*staticPath)
                        : DownstreamPlaceholder::fromSingleDerivedPathBuilt(
                            SingleDerivedPath::Built{.drvPath = makeConstantStorePathRef(drvPath), .output = name})
                              .render();
                }
                auto value = nlohmann::json{{"path", pathText}, {"name", drv.env["name"]}, {"outputs", outputs}};
                return answer(std::string("derivation\0", 11) + value.dump(), 0);
            }
        }

        auto resolved = resolveExprPath(std::move(source));
        auto contents = resolved.resolveSymlinks().readFile();
        std::string encoded = rootName;
        encoded.push_back('\0');
        encoded.append(resolved.path.abs());
        encoded.push_back('\0');
        encoded.append(contents);
        return answer(encoded, 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// `"${./f}"` is the store path cppnix copies the file to, not the source
/// path (eval.cc:2582). The evaluator cannot do this itself: the store is
/// ours, and under read-only mode -- which `nix-instantiate --eval` sets --
/// the answer is the path the copy WOULD produce with no bytes moved.
/// ENG-12447.
static int rustCopyToStore(
    void * ctx,
    const unsigned char * root,
    size_t rootLen,
    const unsigned char * path,
    size_t pathLen,
    const unsigned char ** out,
    size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.copyToStoreAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.copyToStoreAnswer.data());
        *outLen = host.copyToStoreAnswer.size();
        return rc;
    };

    // A C++ exception must not unwind through Rust frames, so every failure
    // comes back as a status and a message instead.
    try {
        NixStringContext context;
        auto sourcePath = rootedPath(host, root, rootLen, path, pathLen);

        if (rootLen > 0)
            ++host.copyMounted;
        else
            ++host.copyAmbient;

        auto storePath = host.state.copyPathToStore(context, sourcePath);
        return answer(host.state.store->printStorePath(storePath), 0);
    } catch (Error & e) {
        // The message alone: the Rust arm carries no source positions
        // (ENG-12137), so a trace block here would be the only difference
        // between the two arms' error text.
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

static int rustStorePath(
    void * ctx,
    const unsigned char * root,
    size_t rootLen,
    const unsigned char * path,
    size_t pathLen,
    const unsigned char ** out,
    size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.storePathAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.storePathAnswer.data());
        *outLen = host.storePathAnswer.size();
        return rc;
    };

    try {
        if (host.state.settings.pureEval)
            host.state.error<EvalError>("'%s' is not allowed in pure evaluation mode", "builtins.storePath")
                .debugThrow();

        std::string_view rootName(reinterpret_cast<const char *>(root), rootLen);
        auto sourcePath = rootedPath(host, root, rootLen, path, pathLen);
        // The store checks and the answer use the visible spelling -- the
        // mount key plus the accessor path, which is what cppnix's storeFS-
        // rooted SourcePath prints -- never the accessor-relative path, which
        // for a mounted input is `/` or `/sub` and belongs to no store.
        auto spelled = [&](const SourcePath & p) {
            if (rootName.empty())
                return p.path.abs();
            return std::string(rootName) + (p.path.isRoot() ? "" : p.path.abs());
        };
        auto visible = spelled(sourcePath);
        if (!host.state.store->isStorePath(visible)) {
            sourcePath = sourcePath.resolveSymlinks(SymlinkResolution::Full);
            visible = spelled(sourcePath);
        }
        if (!host.state.store->isInStore(visible))
            host.state.error<EvalError>("path '%s' is not in the Nix store", visible).debugThrow();

        auto storePath = host.state.store->toStorePath(visible).first;
        auto printedStorePath = host.state.store->printStorePath(storePath);
        // cppnix's rule (`prim_storePath`): a lazily mounted store object is
        // already satisfied by its mount; anything else is ensured unless
        // this process is read-only.
        if (!host.state.storeFS->getMount(CanonPath(printedStorePath)) && !settings.readOnlyMode)
            host.state.store->ensurePath(storePath);

        std::string encoded = printedStorePath;
        encoded.push_back('\0');
        encoded.append(visible);
        return answer(encoded, 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// `builtins.toFile` (`primops.cc:2789`).
///
/// Here rather than in the evaluator for one reason: the *path* is a pure
/// function of the bytes and the references, but whether the bytes are written
/// is `settings.readOnlyMode`, which is ours. `nix-instantiate --eval` computes
/// without writing; `nix build` writes. The evaluator cannot tell those apart
/// and must not guess, so it asks and this answers -- the same seam
/// `rustEnsurePath` uses for the same reason (ENG-12479). ENG-12607.
static int rustStoreText(
    void * ctx,
    const unsigned char * name,
    size_t nameLen,
    const unsigned char * contents,
    size_t contentsLen,
    const unsigned char * references,
    size_t referencesLen,
    const unsigned char ** out,
    size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.storeTextAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.storeTextAnswer.data());
        *outLen = host.storeTextAnswer.size();
        return rc;
    };

    // As in `rustCopyToStore`: a C++ exception must not unwind through Rust
    // frames, so every failure comes back as a status and a message.
    try {
        std::string n(reinterpret_cast<const char *>(name), nameLen);
        std::string c(reinterpret_cast<const char *>(contents), contentsLen);

        // NUL-separated, which is unambiguous because a store path cannot
        // contain one. A trailing partial field would be a malformed call.
        StorePathSet refs;
        std::string_view rest(reinterpret_cast<const char *>(references), referencesLen);
        while (!rest.empty()) {
            auto end = rest.find('\0');
            if (end == std::string_view::npos)
                return answer("rust-eval: unterminated reference in builtins.toFile", 1);
            if (end > 0)
                refs.insert(host.state.store->parseStorePath(rest.substr(0, end)));
            rest.remove_prefix(end + 1);
        }

        auto storePath = settings.readOnlyMode ? host.state.store->makeFixedOutputPathFromCA(
                                                     n,
                                                     TextInfo{
                                                         .hash = hashString(HashAlgorithm::SHA256, c),
                                                         .references = refs,
                                                     })
                                               : ({
                                                     StringSource s{c};
                                                     host.state.store->addToStoreFromDump(
                                                         s,
                                                         n,
                                                         FileSerialisationMethod::Flat,
                                                         ContentAddressMethod::Raw::Text,
                                                         HashAlgorithm::SHA256,
                                                         refs,
                                                         host.state.repair);
                                                 });
        /* cppnix's `prim_toFile` ends in `allowAndSetStorePathString`
           (`primops.cc:2836`): under pure eval the result of
           `builtins.toFile` is readable. Without this line the store path
           comes back unregistered and the Rust arm alone refuses
           `import "${builtins.toFile ...}"` with the AllowList denial.
           `rustWriteDerivations` below shares this body and deliberately
           does NOT allow: cppnix's `writeDerivation` never does. ENG-13138. */
        host.state.allowPath(storePath);
        return answer(host.state.store->printStorePath(storePath), 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// Rust owns ATerm validation, references, store paths, NARs and metadata.
/// This adapter materialises the packet's lazy sources before handing its
/// prepared AddMultipleToStore stream to the store in one operation.
static int rustWriteDerivations(
    void * ctx, const unsigned char * batch, size_t batchLen, const unsigned char ** out, size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.writeDrvAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.writeDrvAnswer.data());
        *outLen = host.writeDrvAnswer.size();
        return rc;
    };

    // As in `rustCopyToStore`: a C++ exception must not unwind through Rust
    // frames, so every failure comes back as a status and a message.
    try {
        auto & store = *host.state.store;

        StringSource source{std::string_view(reinterpret_cast<const char *>(batch), batchLen)};
        const auto count = readNum<uint64_t>(source);
        auto sources = WorkerProto::Serialise<StorePathSet>::read(
            store,
            WorkerProto::ReadConn{
                .from = source,
                .version = {.number = {.major = 1, .minor = 16}},
            });
        // Check the envelope's accounting against the store stream itself.
        const auto streamStart = source.pos;
        if (readNum<uint64_t>(source) != count)
            return answer("rust-eval: derivation batch count does not match its envelope", 1);
        source.pos = streamStart;
        if (settings.readOnlyMode || count == 0)
            return answer("", 0);

        for (const auto & inputSrc : sources)
            host.state.ensureLazyPathCopied(inputSrc);

        store.addMultipleToStore(source, host.state.repair, NoCheckSigs);
        if (source.pos != source.s.size())
            return answer("rust-eval: trailing data after derivation batch", 1);
        host.drvWrites += count;
        ++host.drvFlushes;
        return answer("", 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// Which of a list of store paths are held now: one `queryValidPaths` for
/// the whole list, plus every path at which this evaluator has a tree
/// mounted (`storeFS`: `EvalState::mountInput` mounts every fetched flake
/// input and `fetchTree` result at the store path its content address
/// derives -- SHA-256 NAR hash, or the blake3 tree id for jj -- and never
/// registers it, so the store alone would call the tree the evaluation is
/// reading absent). The
/// evaluation-cache verifier's question (`ixe_valid_paths_fn`) about the
/// objects a witness relies on -- derivations it wrote, outputs it realised,
/// copies and pinned fetches it made; never the trees it read under, whose
/// sealed names pin their records -- so that a served
/// evaluation checks each is still there instead of writing, building or
/// copying it again. Which objects a row relies on is the verifier's business
/// (`readset::validity_lines`); this answers only what is held.
static int
rustValidPaths(void * ctx, const unsigned char * paths, size_t pathsLen, const unsigned char ** out, size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.validPathsAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.validPathsAnswer.data());
        *outLen = host.validPathsAnswer.size();
        return rc;
    };

    // As in `rustCopyToStore`: a C++ exception must not unwind through Rust
    // frames, so every failure comes back as a status and a message.
    try {
        StorePathSet asked;
        std::string encoded;
        std::string_view request(reinterpret_cast<const char *>(paths), pathsLen);
        for (auto & line : tokenizeString<std::vector<std::string>>(request, "\n")) {
            if (host.state.storeFS->getMount(CanonPath(line))) {
                encoded += line;
                encoded.push_back('\n');
            } else {
                asked.insert(host.state.store->parseStorePath(line));
            }
        }
        for (auto & path : host.state.store->queryValidPaths(asked)) {
            encoded += host.state.store->printStorePath(path);
            encoded.push_back('\n');
        }
        return answer(encoded, 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// Whether a path or link target can ride the sealed-paths line format: no
/// control character (the format spends tab and newline, and a decoder that
/// stripped a `\r` would rename a link).
static bool carriable(std::string_view s)
{
    for (unsigned char c : s)
        if (c < 0x20 || c == 0x7f)
            return false;
    return true;
}

/// (path relative to the object, link target), one per symlink under it.
using SealedLinks = std::vector<std::pair<std::string, std::string>>;

/// `links` when every path and target is `carriable`, else `std::nullopt`.
static std::optional<SealedLinks> carriableLinks(SealedLinks links)
{
    for (auto & [rel, target] : links)
        if (!carriable(rel) || !carriable(target))
            return std::nullopt;
    return links;
}

/// Every symlink under `root` of `accessor`, as (path relative to `root`,
/// link target), in one walk; `std::nullopt` when the answer cannot be
/// carried: a directory entry whose type the accessor cannot state, or a
/// path or target that is not `carriable`. Which links leave the object is
/// the verifier's business (`readset::leaving_links`): this walk reports
/// what is there. A jj tree does not come here: `JjTreeAccessor::symlinks`
/// answers for the whole tree in one ABI call, where this walk would be one
/// listing per directory.
static std::optional<SealedLinks> symlinksUnder(SourceAccessor & accessor, const CanonPath & root)
{
    SealedLinks links;
    std::vector<CanonPath> pending;
    pending.push_back(root);
    while (!pending.empty()) {
        auto dir = std::move(pending.back());
        pending.pop_back();
        for (auto & [name, entry] : accessor.readDirectory(dir)) {
            auto child = dir / name;
            auto type = entry;
            if (!type) {
                auto st = accessor.maybeLstat(child);
                if (!st)
                    return std::nullopt;
                type = st->type;
            }
            if (*type == SourceAccessor::tDirectory) {
                pending.push_back(child);
            } else if (*type == SourceAccessor::tSymlink) {
                std::string rel(child.rel());
                auto target = accessor.readLink(child);
                if (!carriable(rel) || !carriable(target))
                    return std::nullopt;
                links.emplace_back(std::move(rel), std::move(target));
            }
        }
    }
    return links;
}

/// Which of a list of store objects are sealed (`ixe_sealed_paths_fn`): held
/// by the store and content-addressed, checked as `isContentAddressed` does
/// (the path is the one its content address derives, so a stray `ca` field
/// on an input-addressed object does not qualify; `isSelfCertifying` is not
/// the test, since it excludes the jj tree address, which cppnix cannot
/// recompute but which is a function of the content all the same). Each
/// sealed object's line carries every symlink under it as tab-separated
/// (relative path, target) pairs. Asked by the evaluation-cache verifier
/// once per object; a sealed object is never asked about again, so the walk
/// is paid once per object ever.
static int
rustSealedPaths(void * ctx, const unsigned char * paths, size_t pathsLen, const unsigned char ** out, size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.sealedPathsAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.sealedPathsAnswer.data());
        *outLen = host.sealedPathsAnswer.size();
        return rc;
    };

    // As in `rustCopyToStore`: a C++ exception must not unwind through Rust
    // frames, so every failure comes back as a status and a message.
    try {
        auto & store = *host.state.store;
        std::string_view request(reinterpret_cast<const char *>(paths), pathsLen);
        std::string encoded;
        // The walk is paid once per object, at the run that first sees it:
        // the instrument for a slow record (round 15b: 30s over one fresh
        // `path:./ix` tree of 191k entries), one line per object walked.
        static const bool trace = getEnv("IXE_REPLAY_TRACE").has_value();
        for (auto & line : tokenizeString<std::vector<std::string>>(request, "\n")) {
            StorePath object{line};
            std::shared_ptr<SourceAccessor> accessor;
            bool mounted = false;
            if (auto mount = host.state.storeFS->getMount(CanonPath(store.printStorePath(object)))) {
                // A tree this evaluator mounted at its store-path name (a
                // `path:` or `jj+file` flake input): the fetcher named it from
                // its content and holds it for the process; the store never
                // registered it. The mount is the accessor to walk.
                accessor = mount;
                mounted = true;
            } else {
                if (!store.isValidPath(object)) {
                    if (trace)
                        std::cerr << "ixe sealing: " << line << " skipped: not valid, not mounted\n";
                    continue;
                }
                if (!store.queryPathInfo(object)->isContentAddressed(store)) {
                    if (trace)
                        std::cerr << "ixe sealing: " << line << " skipped: input-addressed\n";
                    continue;
                }
                accessor = store.getFSAccessor(object);
                if (!accessor)
                    continue;
            }
            auto started = std::chrono::steady_clock::now();
            // A jj mount answers for the whole tree in one ABI call
            // (`jjt_tree_symlinks`; the per-directory walk was 14,664
            // listings and 1.74 s of the 1.84 s an edit paid here, bed
            // l2cb1); every other accessor is walked directory by directory.
            // The trace separates the ABI's time from the rest, and says when
            // it cannot (a wrapper around the jj accessor hides the
            // repository).
            auto * jjTree = dynamic_cast<nix::fetchers::JjTreeAccessor *>(accessor.get());
            auto abiBefore = jjTree ? jjTree->abiStats() : nix::fetchers::JjTreeRepo::AbiStats{};
            auto links = jjTree ? carriableLinks(jjTree->symlinks()) : symlinksUnder(*accessor, CanonPath::root);
            if (trace) {
                auto ns =
                    std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now() - started)
                        .count();
                std::cerr << "ixe sealing: " << line << (mounted ? " mount " : " store ") << ns
                          << "ns links=" << (links ? std::to_string(links->size()) : std::string("uncarriable"))
                          << " accessor=" << accessor->identityClass(CanonPath::root);
                if (jjTree) {
                    auto abiAfter = jjTree->abiStats();
                    std::cerr << " abi_listings=" << (abiAfter.listings - abiBefore.listings)
                              << " abi_list_ns=" << (abiAfter.listNs - abiBefore.listNs)
                              << " abi_walks=" << (abiAfter.walks - abiBefore.walks)
                              << " abi_walk_ns=" << (abiAfter.walkNs - abiBefore.walkNs);
                } else {
                    std::cerr << " abi_listings=unknown(not a JjTreeAccessor)";
                }
                std::cerr << "\n";
            }
            if (!links)
                continue;
            encoded += line;
            for (auto & [rel, target] : *links) {
                encoded.push_back('\t');
                encoded += rel;
                encoded.push_back('\t');
                encoded += target;
            }
            encoded.push_back('\n');
        }
        return answer(encoded, 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// `ixe_allow_paths_fn`: `allowPath` for each NUL-terminated store path, what
/// the copy, fetch and tree hooks end with (`allowAndSetStorePathString`,
/// `fetchTree`'s `allowPath`), for an effect the witness verifier served by
/// validity. Not `allowClosure`: a copy allows its own path and nothing it
/// references; `rustRealiseAllow` below is the closure form, for realised
/// outputs, as `realiseContext` allows them.
static int rustAllowPaths(
    void * ctx, const unsigned char * request, size_t requestLen, const unsigned char ** out, size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.allowPathsAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.allowPathsAnswer.data());
        *outLen = host.allowPathsAnswer.size();
        return rc;
    };

    try {
        std::string_view rest(reinterpret_cast<const char *>(request), requestLen);
        while (!rest.empty()) {
            auto end = rest.find('\0');
            if (end == std::string_view::npos)
                return answer("rust-eval: unterminated store path in allow request", 1);
            if (end > 0)
                host.state.allowPath(host.state.store->parseStorePath(rest.substr(0, end)));
            rest.remove_prefix(end + 1);
        }
        return answer("", 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// `builtins.path` (`primops.cc`, `addPath`), whose store work this performs.
///
/// The evaluator has already walked the tree and run the filter -- the filter
/// is a Nix function and only the interpreter can call it -- so what arrives
/// is a set of accepted paths. This turns that set into a `PathFilter` and
/// makes the one call `addPath` makes, `EvalState::addPathToStore`: the
/// tree-id road for a jj-served directory, the NAR road otherwise, the same
/// expected-hash check. Re-deciding anything here would be a second
/// implementation for the two arms to disagree over. ENG-12678.
static int rustStoreFiltered(
    void * ctx, const unsigned char * request, size_t requestLen, const unsigned char ** out, size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.storeFilteredAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.storeFilteredAnswer.data());
        *outLen = host.storeFilteredAnswer.size();
        return rc;
    };

    // As in `rustCopyToStore`: a C++ exception must not unwind through Rust
    // frames, so every failure comes back as a status and a message.
    try {
        // NUL-terminated fields, the encoding documented beside
        // `ixe_store_filtered_fn` in ixe.h. A missing terminator is a
        // malformed call and not a field with the rest of the buffer in it.
        std::vector<std::string_view> fields;
        std::string_view rest(reinterpret_cast<const char *>(request), requestLen);
        while (!rest.empty()) {
            auto end = rest.find('\0');
            if (end == std::string_view::npos)
                return answer("rust-eval: unterminated field in builtins.path request", 1);
            fields.emplace_back(rest.substr(0, end));
            rest.remove_prefix(end + 1);
        }
        if (fields.size() < 7)
            return answer("rust-eval: builtins.path request is too short", 1);

        std::string root = fields[0].empty()
                               ? std::string(fields[1])
                               : std::string(fields[0]) + (fields[1] == "/" ? std::string() : std::string(fields[1]));
        std::string name(fields[2]);

        ContentAddressMethod method = ContentAddressMethod::Raw::NixArchive;
        if (fields[3] == "flat")
            method = ContentAddressMethod::Raw::Flat;
        else if (fields[3] != "nar")
            return answer("rust-eval: unknown ingestion method in builtins.path request", 1);

        std::optional<Hash> expectedHash;
        if (!fields[4].empty())
            expectedHash = Hash::parseAny(fields[4], HashAlgorithm::SHA256);

        // `addPath`'s store-path branch (`primops.cc:2947`). The evaluator
        // decides *whether* it applies -- the root coerced with a context and
        // is under the store directory -- because only it saw the value, and
        // it has already realised that context and rewritten the root. What
        // is left is the store query, which is only ours to make.
        bool inheritReferences = false;
        if (fields[5] == "inherit-references")
            inheritReferences = true;
        else if (fields[5] != "own-references")
            return answer("rust-eval: unknown reference marker in builtins.path request", 1);

        // Membership, not a predicate the evaluator described: the accepted
        // set is closed downwards, so a directory that is absent prunes its
        // whole subtree and `dumpPath` never asks about anything below it.
        std::optional<StringSet> accepted;
        if (fields[6] == "filtered") {
            if ((fields.size() - 7) % 2 != 0)
                return answer("rust-eval: builtins.path request has a half entry", 1);
            StringSet paths;
            for (size_t i = 7; i < fields.size(); i += 2)
                paths.insert(std::string(fields[i]));
            accepted = std::move(paths);
        } else if (fields[6] != "unfiltered")
            return answer("rust-eval: unknown filter marker in builtins.path request", 1);

        std::unique_ptr<PathFilter> filter;
        if (accepted)
            filter = std::make_unique<PathFilter>([&](const std::string & p) { return accepted->count(p) > 0; });

        auto & store = *host.state.store;
        auto sourcePath = rootedPath(
            host,
            reinterpret_cast<const unsigned char *>(fields[0].data()),
            fields[0].size(),
            reinterpret_cast<const unsigned char *>(fields[1].data()),
            fields[1].size());

        // As `addPath`: a root whose store object went away between the
        // realise and this call inherits nothing; any other failure of the
        // query is the store's and propagates, never a copy with no
        // references.
        StorePathSet refs;
        if (inheritReferences) {
            auto [storePath, subPath] = store.toStorePath(root);
            try {
                refs = store.queryPathInfo(storePath)->references;
            } catch (InvalidPath &) {
            }
        }

        /* The one call every arm makes (`EvalState::addPathToStore`): it
           allows the path too, which under pure eval is what makes the
           result of `builtins.path`/`builtins.filterSource` readable. That
           omission was once 96 of the 100 rust-arm failures in the whole-ix
           sweep: every `lib.cleanSource`d tree came back unregistered, and
           the first read inside it got the AllowList denial. ENG-13138. */
        auto dstPath =
            host.state.addPathToStore(sourcePath.resolveSymlinks(), name, method, filter.get(), expectedHash, refs);
        return answer(store.printStorePath(dstPath), 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// `builtins.fetchurl` and `builtins.fetchTarball`, from `checkURI` onward.
///
/// The evaluator has already done everything cppnix's `fetch()`
/// (`primops/fetchTree.cc:462`) does before it touches the world: read the
/// argument set, rewrite a `channel:` URL, default the name and validate it,
/// parse the `sha256`. What is left is the IO, and this is a transcription of
/// cppnix's own -- same `ensurePath` early exit, same `downloadFile` and
/// `downloadTarball`, same mismatch check and same message. Nothing here
/// re-derives a name or a URL; doing so would be a second implementation of
/// rules the evaluator already applied, for the two to disagree over.
static int
rustFetch(void * ctx, const unsigned char * request, size_t requestLen, const unsigned char ** out, size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.fetchAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.fetchAnswer.data());
        *outLen = host.fetchAnswer.size();
        return rc;
    };

    // As in `rustCopyToStore`: a C++ exception must not unwind through Rust
    // frames, so every failure comes back as a status and a message.
    try {
        // NUL-terminated fields, the encoding documented beside
        // `ixe_fetch_fn` in ixe.h.
        std::vector<std::string_view> fields;
        std::string_view rest(reinterpret_cast<const char *>(request), requestLen);
        while (!rest.empty()) {
            auto end = rest.find('\0');
            if (end == std::string_view::npos)
                return answer("rust-eval: unterminated field in fetch request", 1);
            fields.emplace_back(rest.substr(0, end));
            rest.remove_prefix(end + 1);
        }
        if (fields.size() != 4)
            return answer("rust-eval: fetch request must have four fields", 1);

        std::string url(fields[0]);
        std::string name(fields[1]);

        bool unpack = false;
        if (fields[2] == "tarball")
            unpack = true;
        else if (fields[2] != "file")
            return answer("rust-eval: unknown kind in fetch request", 1);

        std::optional<Hash> expectedHash;
        if (!fields[3].empty())
            expectedHash = Hash::parseAny(fields[3], HashAlgorithm::SHA256);

        // Reached only when the evaluator asked, and it refuses the whole
        // question channel under restrict-eval, so in this build this is
        // belt and braces. Kept because it is cppnix's check and this is
        // cppnix's code path: the day the evaluator learns to distinguish
        // the two purity settings, the check has to already be here.
        host.state.checkURI(url);

        auto & store = *host.state.store;

        // Early exit if pinned and already in the store. THE hermetic branch:
        // with a sha256 the store path is known before anything is
        // downloaded, and if the store can produce it nothing is.
        if (expectedHash && expectedHash->algo == HashAlgorithm::SHA256) {
            auto expectedPath = store.makeFixedOutputPath(
                name,
                FixedOutputInfo{
                    .method = unpack ? FileIngestionMethod::NixArchive : FileIngestionMethod::Flat,
                    .hash = *expectedHash,
                    .references = {}});
            try {
                store.ensurePath(expectedPath);
                host.state.allowPath(expectedPath);
                return answer(store.printStorePath(expectedPath), 0);
            } catch (Error & e) {
                debug(
                    "substitution of '%s' failed, will try to download: %s",
                    store.printStorePath(expectedPath),
                    e.what());
                // Fall through to download.
            }
        }

        auto storePath = unpack ? fetchToStore(
                                      host.state.fetchSettings,
                                      store,
                                      fetchers::downloadTarball(store, host.state.fetchSettings, url),
                                      FetchMode::Copy,
                                      name)
                                : fetchers::downloadFile(store, host.state.fetchSettings, url, name).storePath;

        if (expectedHash) {
            auto hash = unpack ? store.queryPathInfo(storePath)->narHash
                               : hashPath(
                                     {store.requireStoreObjectAccessor(storePath)},
                                     FileSerialisationMethod::Flat,
                                     HashAlgorithm::SHA256)
                                     .hash;
            if (hash != *expectedHash)
                return answer(
                    fmt("hash mismatch in file downloaded from '%s':\n  specified: %s\n  got:       %s",
                        url,
                        expectedHash->to_string(HashFormat::Nix32, true),
                        hash.to_string(HashFormat::Nix32, true)),
                    1);
        }

        host.state.allowPath(storePath);
        return answer(store.printStorePath(storePath), 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// Defined with the rest of the flake machinery, far below; declared here
/// because `rustLockFlake` and `rustEvaluandOf` are the two callers and they
/// sit at opposite ends of this file. One definition on purpose: the overrides
/// document is what decides which tree every flake input resolves to, and two
/// copies of it is how `getFlake` and the command line come to disagree.
/// What `call-flake.nix` is applied to: the lock file's text and the
/// overrides document. One body for the evaluand the commands build and for
/// the answer `builtins.getFlake` hands the evaluator, because the two are
/// the same description and a read set digests it. Defined beside
/// `flakeOverridesJSON` below.
struct LockedFlakeDescription
{
    std::string lockFile;
    nlohmann::json overrides;
};

static LockedFlakeDescription describeLockedFlake(EvalState & state, const flake::LockedFlake & lockedFlake);

/// `builtins.getFlake`, from `parseFlakeRef` up to but not including
/// `callFlake`.
///
/// This is the first half of cppnix's `prim_getFlake`
/// (`libflake/flake-primops.cc`), transcribed rather than reinvented, and the
/// line it stops at is the line the charter draws: everything here is IO and
/// policy -- parsing the reference, the pure-eval rule, the registry, the
/// input-graph walk, the fetches -- and `callFlake` itself is an ordinary Nix
/// application the VM performs. What crosses back is a lock file and an
/// overrides document, i.e. data, exactly as it does for the `<flake>#attr`
/// command line.
///
/// **One seam, two ways in.** `rustEvaluandOf` builds the same three
/// arguments for the command line. Both call `flake::callFlakeSource()` and
/// `flakeOverridesJSON`, so a change to either reaches both, which is the
/// property `rust-flake-entry.md` asks for and the reason `getFlake` is not a
/// second implementation of flake evaluation.
///
/// The one difference from the command line is the `LockFlags`, and it is
/// cppnix's difference rather than this backend's: `prim_getFlake` never
/// updates or writes a lock file and decides `useRegistries` and
/// `allowUnlocked` off `pureEval`, where the command line takes the flags the
/// user's command line built.
static int rustLockFlake(
    void * ctx, const unsigned char * flakeRefPtr, size_t flakeRefLen, const unsigned char ** out, size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.lockFlakeAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.lockFlakeAnswer.data());
        *outLen = host.lockFlakeAnswer.size();
        return rc;
    };

    /* The same refusal `rustFetchTree` makes, for the same reason and in the
       same words: `emitTreeAttrs` wraps every metadata attribute in a
       per-attribute recording thunk when the tracker is on, and
       `flakeOverridesJSON` below forces every one of them. Serialising them
       would record reads the flake never made and hand the evaluator plain
       values that can never record the ones it does. Both are silent, so a
       tracked evaluation gets a named refusal instead. */
    if (host.state.readSetTracker)
        return answer(
            "builtins.getFlake while the read-set tracker is on (the overrides this hands over "
            "are cppnix's emitTreeAttrs sets, which are per-attribute recording thunks under the "
            "tracker, and serialising them would both record reads nobody made and lose the ones "
            "the flake does make)",
            2);

    // As in `rustFetchTree`: a C++ exception must not unwind through Rust
    // frames, so every failure comes back as a status and a message.
    try {
        auto & state = host.state;
        std::string flakeRefS(reinterpret_cast<const char *>(flakeRefPtr), flakeRefLen);
        auto flakeRef = parseFlakeRef(state.fetchSettings, flakeRefS, {}, true);

        /* cppnix's own rule, raised here so the message is cppnix's. The
           position it prints is the one thing that cannot be transcribed: this
           backend has no positions yet (ENG-12137), so the `at %s` clause is
           dropped rather than filled with a made-up one. */
        if (state.settings.pureEval && !flakeRef.input.isLocked(state.fetchSettings))
            return answer(
                fmt("cannot call 'getFlake' on unlocked flake reference '%s' (use '--impure' to override)", flakeRefS),
                1);

        /* The ix-local backwards-compatibility branch `prim_getFlake` carries,
           kept in step with it: a lazily mounted store path has to be
           materialised before the path-input fetch can read it from disk. */
        if (auto sourcePath = flakeRef.input.getSourcePath();
            flakeRef.input.getType() == "path" && sourcePath && state.store->isInStore(sourcePath->string())) {
            auto [storePath, subPath] = state.store->toStorePath(sourcePath->string());
            state.ensureLazyPathCopied(storePath);
        }

        /* The same locker the command line uses. It reads each `flake.nix`
           as a document on the selected backend (`flake::readFlake`, through
           the producer this file installs), so a `getFlake` run reports
           `evaluator: rust` with nothing exempted. */
        auto lockedFlake = std::make_shared<flake::LockedFlake>(flake::lockFlake(
            flakeSettings,
            state,
            flakeRef,
            flake::LockFlags{
                .updateLockFile = false,
                .writeLockFile = false,
                .useRegistries = !state.settings.pureEval && flakeSettings.useRegistries,
                .allowUnlocked = !state.settings.pureEval,
            }));

        auto description = describeLockedFlake(state, *lockedFlake);
        nlohmann::json doc = nlohmann::json::object({
            {"source", std::string(flake::callFlakeSource())},
            {"lockFile", description.lockFile},
            // A string holding a document, not a nested object: see the note
            // beside `ixe_lock_flake_fn` in ixe.h. A read set digests these
            // bytes, and a re-serialisation on the far side would put key
            // ordering between what was produced and what is digested.
            {"overrides", description.overrides.dump()},
        });
        return answer(doc.dump(), 0);
    } catch (Error & e) {
        /* `message()`, like every other hook here, and the reason is at the
           first of them: the Rust arm carries no source positions (ENG-12137),
           so a trace block would be the only difference between the two arms'
           error text. `what()` additionally renders the whole `ErrorInfo`,
           "error: " prefix and all, on top of the prefix the evaluator adds --
           a `MissingExperimentalFeature` through here reads `error: error:
           experimental Nix feature 'flakes' is disabled`.

           This comment used to claim the other hooks had that bug and cite a
           ticket for sweeping them. They did not: every `catch (Error &)` in
           this file already used `message()`, and the `what()` calls a grep
           turns up are the `catch (std::exception &)` clauses beneath them,
           where `what()` is correct because a plain `std::exception` renders
           no prefix. The one defective site was this one, written against the
           pattern. `rust-nix-eval-gate.sh` now asserts the prefix appears
           exactly once, which is what would have caught it (ENG-13022). */
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// `builtins.parseFlakeRef`: the flake-ref grammar, which lives here because
/// it is cppnix's `parseFlakeRef` and nothing else -- URL and path syntax,
/// registry shorthands, the attrs each scheme admits. The evaluator sends the
/// reference string and receives the parsed reference as a flat JSON object
/// (`toAttrs()` through `fetchers::attrsToJSON`, scalars only), which it
/// turns back into the attribute set the program sees.
///
/// The `flakes` experimental-feature check runs here and not in the
/// evaluator, mirroring where cppnix makes it: `flake-primops.cc` registers
/// the primop unconditionally with `.experimentalFeature = Xp::Flakes`, so
/// `builtins ? parseFlakeRef` is `true` with the feature off and only a call
/// raises `MissingExperimentalFeature`. `require` throws that same error, and
/// it travels back as an ordinary failure with cppnix's message.
///
/// One ordering edge is accepted rather than mirrored: cppnix's stub raises
/// before the argument is forced, where the evaluator forces first and this
/// gate runs only when the question arrives, so "feature off AND argument
/// invalid" reports the argument on the rust arm. See `bi_parse_flake_ref`
/// in `primops_host.rs` for why that is left alone.
static int rustParseFlakeRef(
    void * ctx, const unsigned char * flakeRefPtr, size_t flakeRefLen, const unsigned char ** out, size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.parseFlakeRefAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.parseFlakeRefAnswer.data());
        *outLen = host.parseFlakeRefAnswer.size();
        return rc;
    };

    // As everywhere in this file: a C++ exception must not unwind through
    // Rust frames, so every failure comes back as a status and a message.
    try {
        experimentalFeatureSettings.require(Xp::Flakes);
        std::string flakeRefS(reinterpret_cast<const char *>(flakeRefPtr), flakeRefLen);
        // cppnix's own call, arguments and all (`prim_parseFlakeRef`,
        // flake-primops.cc:103): no base directory, `allowMissing = true`.
        auto attrs = parseFlakeRef(host.state.fetchSettings, flakeRefS, {}, true).toAttrs();
        return answer(fetchers::attrsToJSON(attrs).dump(), 0);
    } catch (Error & e) {
        // `message()`, not `what()`; the reason is written out at
        // `rustLockFlake`'s catch clause.
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// `builtins.flakeRefToString`, the grammar's other direction:
/// `FlakeRef::fromAttrs(...).to_string()`. The evaluator has already forced
/// the set and raised cppnix's negative-integer and wrong-type errors on its
/// side, so what arrives is a bag of scalars -- `ixe_fetch_tree_fn`'s triplet
/// encoding without its leading fetcher field.
///
/// Feature gate here for the same reason as `rustParseFlakeRef` above.
static int rustFlakeRefToString(
    void * ctx, const unsigned char * request, size_t requestLen, const unsigned char ** out, size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.flakeRefToStringAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.flakeRefToStringAnswer.data());
        *outLen = host.flakeRefToStringAnswer.size();
        return rc;
    };

    try {
        experimentalFeatureSettings.require(Xp::Flakes);

        // NUL-terminated fields, the encoding documented beside
        // `ixe_flake_ref_to_string_fn` in ixe.h.
        std::vector<std::string_view> fields;
        std::string_view rest(reinterpret_cast<const char *>(request), requestLen);
        while (!rest.empty()) {
            auto end = rest.find('\0');
            if (end == std::string_view::npos)
                return answer("rust-eval: unterminated field in flake ref request", 1);
            fields.emplace_back(rest.substr(0, end));
            rest.remove_prefix(end + 1);
        }
        if (fields.size() % 3 != 0)
            return answer("rust-eval: flake ref request has a partial attribute", 1);

        fetchers::Attrs attrs;
        for (size_t i = 0; i < fields.size(); i += 3) {
            std::string name(fields[i]);
            auto tag = fields[i + 1];
            std::string text(fields[i + 2]);
            if (tag == "s") {
                attrs.emplace(name, text);
            } else if (tag == "b") {
                attrs.emplace(name, Explicit<bool>{text == "1"});
            } else if (tag == "i") {
                attrs.emplace(name, static_cast<uint64_t>(std::stoull(text)));
            } else
                return answer("rust-eval: unknown attribute tag in flake ref request", 1);
        }

        return answer(FlakeRef::fromAttrs(host.state.fetchSettings, attrs).to_string(), 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// `builtins.fetchTree` and `builtins.fetchGit`, from `Input::fromAttrs`
/// onward.
///
/// The evaluator forced and classified the input attributes and raised the
/// errors a program can see; what arrives is the bag. Everything here is how
/// an `Input` is built and fetched, and it is cppnix's own code path in
/// cppnix's own order: `fixGitURL`, the `exportIgnore` and `shallow`
/// defaults, `Input::fromAttrs`, the registry, the locked-input check,
/// `checkURI`, the input cache, `mountInput` and `emitTreeAttrs`.
///
/// The answer is JSON rather than a store path because the attribute set has
/// no fixed shape. Each attribute's value is rendered by `printValueAsJSON`,
/// so there is one serialiser rather than a second hand-written one -- but
/// the set is assembled here rather than passed to it whole; see the comment
/// at the loop for what that avoids.
static int
rustFetchTree(void * ctx, const unsigned char * request, size_t requestLen, const unsigned char ** out, size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.fetchTreeAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.fetchTreeAnswer.data());
        *outLen = host.fetchTreeAnswer.size();
        return rc;
    };

    /* The one thing this cannot serve, and it must say so rather than serve it
       wrong. `emitTreeAttrs` wraps every metadata attribute in a per-attribute
       recording thunk when the tracker is on (`allocRecordedTreeAttr`,
       primops/fetchTree.cc:41), so that an entry which never reads the
       revision does not acquire it as an input. Two things go wrong if this
       proceeds: the JSON below FORCES every one of those thunks, recording
       reads the program never made, and the evaluator receives plain values
       that can never record the reads it does make. Both are silent. So a
       tracked evaluation gets a named refusal instead. */
    if (host.state.readSetTracker)
        return answer(
            "the read-set tracker is on, and a tree fetch cannot carry its per-attribute "
            "recording through this backend: cppnix returns a thunk per metadata attribute "
            "that records the read when it is forced, and serialising them here would both "
            "force reads nobody made and lose the ones they do",
            2);

    // As in `rustCopyToStore`: a C++ exception must not unwind through Rust
    // frames, so every failure comes back as a status and a message.
    try {
        // NUL-terminated fields, the encoding documented beside
        // `ixe_fetch_tree_fn` in ixe.h.
        std::vector<std::string_view> fields;
        std::string_view rest(reinterpret_cast<const char *>(request), requestLen);
        while (!rest.empty()) {
            auto end = rest.find('\0');
            if (end == std::string_view::npos)
                return answer("rust-eval: unterminated field in tree fetch request", 1);
            fields.emplace_back(rest.substr(0, end));
            rest.remove_prefix(end + 1);
        }
        if (fields.empty())
            return answer("rust-eval: empty tree fetch request", 1);

        bool isFetchGit = false;
        bool isFinal = false;
        if (fields[0] == "fetchGit")
            isFetchGit = true;
        else if (fields[0] == "fetchFinalTree")
            isFinal = true;
        else if (fields[0] != "fetchTree")
            return answer("rust-eval: unknown fetcher in tree fetch request", 1);
        // cppnix's `fetcher` local, which is derived from `isFetchGit` alone
        // (`fetchTree.cc:186`), so a final fetch reports itself as
        // `fetchTree` in every message. The wire spelling and the message
        // spelling differ on purpose; see `TreeFetcher::error_name`.
        auto fetcher = std::string(isFetchGit ? "fetchGit" : "fetchTree");

        if ((fields.size() - 1) % 3 != 0)
            return answer("rust-eval: tree fetch request has a partial attribute", 1);

        fetchers::Attrs attrs;
        for (size_t i = 1; i < fields.size(); i += 3) {
            std::string name(fields[i]);
            auto tag = fields[i + 1];
            std::string text(fields[i + 2]);
            if (tag == "s") {
                // fixGitURL lives here and not in the evaluator: it is URL
                // parsing and re-rendering, it decides a store path, and
                // `GitInputScheme::fromAttrs` applies it too (git.cc:493).
                attrs.emplace(name, isFetchGit && name == "url" ? fixGitURL(text).to_string() : text);
            } else if (tag == "b") {
                attrs.emplace(name, Explicit<bool>{text == "1"});
            } else if (tag == "i") {
                attrs.emplace(name, static_cast<uint64_t>(std::stoull(text)));
            } else
                return answer("rust-eval: unknown attribute tag in tree fetch request", 1);
        }

        // cppnix's two default injections, kept on this side with
        // Input::fromAttrs because they are how the input is built.
        if (isFetchGit && !attrs.contains("exportIgnore")
            && (!attrs.contains("submodules") || !*fetchers::maybeGetBoolAttr(attrs, "submodules"))) {
            attrs.emplace("exportIgnore", Explicit<bool>{true});
        }
        auto type = fetchers::maybeGetStrAttr(attrs, "type");
        if (type == "git" && !isFetchGit && !attrs.contains("shallow")
            && !fetchers::maybeGetBoolAttr(attrs, "exportHistory").value_or(false)) {
            attrs.emplace("shallow", Explicit<bool>{true});
        }

        auto input = fetchers::Input::fromAttrs(host.state.fetchSettings, std::move(attrs));

        auto & state = host.state;
        // cppnix's registry resolution, kept in lockstep (fetchTree.cc,
        // prim_fetchTree: same condition, same Limited lookup, and both
        // arms discard the registry entry's extra attributes -- `dir` is a
        // flake-layer notion that fetchTree never had; the flake layer
        // applies it in getFlake via InputCache's per-call lookup).
        if (!state.settings.pureEval && !input.isDirect() && experimentalFeatureSettings.isEnabled(Xp::Flakes))
            input =
                lookupInRegistries(state.fetchSettings, *state.store, input, fetchers::UseRegistries::Limited).first;

        if (state.settings.pureEval && !input.isLocked(state.fetchSettings)) {
            if (input.getNarHash() || input.getTreeHash())
                warn(
                    "Input '%s' is unlocked (e.g. lacks a Git revision) but is checked by content hash. "
                    "This is not reproducible and will break after garbage collection or when shared.",
                    input.to_string());
            else
                return answer(
                    fmt("in pure evaluation mode, '%s' doesn't fetch unlocked input '%s'", fetcher, input.to_string()),
                    1);
        }

        state.checkURI(input.toURLString());

        // cppnix's `params.isFinal` branch. A final fetch marks the input;
        // a plain one rejects an input that already carries the mark.
        if (isFinal)
            input.attrs.insert_or_assign("__final", Explicit<bool>(true));
        else if (input.isFinal())
            return answer(fmt("input '%s' is not allowed to use the '__final' attribute", input.to_string()), 1);

        auto cachedInput =
            state.inputCache->getAccessor(state.fetchSettings, *state.store, input, fetchers::UseRegistries::No);
        auto storePath = state.mountInput(cachedInput.lockedInput, input, cachedInput.accessor);

        Value v;
        // The `revCount = 0` fallback is fetchGit's, which is why the fetcher travels.
        emitTreeAttrs(state, storePath, cachedInput.lockedInput, v, isFetchGit, false);

        /* Attribute by attribute, NOT `printValueAsJSON` over the whole set.
           That function collapses any attrset carrying an `outPath` to that
           string alone (value-to-json.cc:100) -- the derivation shorthand --
           so serialising the set as a unit answers with a bare JSON string and
           loses every other attribute. It did exactly that on the first run:
           26 of 36 gate cases failed with "a fetched tree did not answer with
           an attribute set", which is the evaluator refusing to guess rather
           than accepting a string where a set belongs.

           Per attribute is immune because the shorthand fires on a *set* that
           has an `outPath` member, and these values are scalars and, for
           `history`, a set that has no such member. */
        state.forceAttrs(v, noPos, "while serialising a fetched tree");
        nlohmann::json treeJson = nlohmann::json::object();
        for (auto & a : *v.attrs()) {
            NixStringContext context;
            treeJson[std::string(state.symbols[a.name])] =
                printValueAsJSON(state, true, *a.value, noPos, context, false);
        }
        return answer(treeJson.dump(), 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// cppnix's four spellings for a directory entry type, which is what
/// `builtins.readDir` and `builtins.readFileType` return (`primops.cc:2480`).
/// One copy, used by both hooks below, because two would be two chances to
/// disagree with the decoder on the Rust side.
///
/// Not `SourceAccessor::Stat::typeString`, which is a *different* mapping for
/// a different audience: it spells the exotic node types out ("character
/// device", "fifo") for a diagnostic, where `fileTypeToString` folds all of
/// them into "unknown" because that is the value a Nix program sees. Using
/// the diagnostic one here would make `builtins.readFileType "/dev/null"`
/// answer "character device" on this backend and "unknown" on cppnix.
static std::string_view fileTypeName(SourceAccessor::Type type)
{
    // Every enumerator named rather than a `default`, which is what
    // `-Werror=switch-enum` asks for and is worth having here: cppnix's
    // `fileTypeToString` folds the exotic node types into "unknown" behind a
    // `default`, so a node type added upstream would silently become
    // "unknown" on both arms. Naming them makes the next one stop this file
    // compiling until somebody decides, which is the louder half of the same
    // answer.
    switch (type) {
    case SourceAccessor::tRegular:
        return "regular";
    case SourceAccessor::tDirectory:
        return "directory";
    case SourceAccessor::tSymlink:
        return "symlink";
    // What `fileTypeToString` answers for all of these: "unknown" is the
    // value a Nix program sees for a device node, a socket or a fifo.
    case SourceAccessor::tChar:
    case SourceAccessor::tBlock:
    case SourceAccessor::tSocket:
    case SourceAccessor::tFifo:
    case SourceAccessor::tUnknown:
        return "unknown";
    }
    // Unreachable for a well-formed value, and required: a `switch` over an
    // enum is not a total function in C++, and falling off the end without
    // returning is undefined behaviour rather than a compile error.
    return "unknown";
}

/// `builtins.readFile`, and the bytes half of an `import`.
///
/// Here rather than in the evaluator because `pure-eval` and `restrict-eval`
/// are enforced by the accessor: cppnix wraps `rootFS` in an
/// `AllowListSourceAccessor` when either is set (`eval.cc:306`), so a read
/// that does not go through `rootFS` cannot honour them. The evaluator's own
/// `std::fs` reader consults no allow list, so before this hook existed it
/// refused all six plain reads under either setting rather than answering
/// outside the list -- which made no flake evaluable on this backend, since
/// flake entry means importing files out of a fetched store path. ENG-12792.
///
/// A transcription of `prim_readFile` (`primops.cc:2201`) from `realisePath`
/// onward: the evaluator has already coerced the value to a path and realised
/// its context, so what is left is the resolution, `rootPath` and the read.
/// Nothing here re-decides which path to read.
///
/// `resolveSymlinks` is the part that is easy to drop and was
/// (ENG-12871). `realisePath`'s `resolveSymlinks` argument defaults to
/// `SymlinkResolution::Full` (`eval.hh:1133`) and `prim_readFile` takes the
/// default, so this is `Full` too. It is not optional politeness:
/// `PosixSourceAccessor::readFile` opens with `O_NOFOLLOW` behind an
/// `assertNoSymlinks` over the whole path (`posix-source-accessor.cc:42`), so
/// without the resolution a symlink is an error rather than a slower read.
/// cppnix puts symlink following in `EvalState::realisePath` on purpose and
/// keeps the accessor strict, which means every caller has to say what it
/// wants -- and the resolution runs *through this accessor*, so `pure-eval`
/// and `restrict-eval` apply to each component it walks exactly as they do in
/// cppnix.
///
/// `prim_readFile`'s reference scan is deliberately not transcribed. It gives
/// the resulting *string* the store references found in the bytes, and this
/// boundary carries bytes and not contexts; the evaluator's own
/// `NeedPath::Contents` answer is a plain string on both arms today, so
/// adding half of the scan here would be a divergence rather than a fix. It
/// belongs with the read set's own context work (ENG-12465).
static int rustReadFile(
    void * ctx,
    const unsigned char * root,
    size_t rootLen,
    const unsigned char * path,
    size_t pathLen,
    const unsigned char ** out,
    size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.readFileAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.readFileAnswer.data());
        *outLen = host.readFileAnswer.size();
        return rc;
    };

    // As in `rustCopyToStore`: a C++ exception must not unwind through Rust
    // frames, so every failure comes back as a status and a message. A
    // RestrictedPathError arrives here like any other Error and goes back as
    // its own text, which is right: cppnix does not make one catchable
    // either, since `prim_tryEval` catches `AssertionError` only
    // (`primops.cc:1219`).
    try {
        return answer(
            rootedPath(host, root, rootLen, path, pathLen).resolveSymlinks(SymlinkResolution::Full).readFile(), 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// `builtins.pathExists`.
///
/// A transcription of `prim_pathExists` (`primops.cc:2081`). cppnix turns a
/// forbidden path into `false` and a missing one into `false` through
/// `maybeLstat`. Every other exception propagates. The answer buffer keeps
/// those three outcomes distinct across the ABI and in replay digests.
///
/// The trailing-slash branch of `prim_pathExists` is the separate
/// `rustDirExists` operation below: it inspects the *value*
/// (`primops.cc:2088`), and only a string argument can end in `/`. The
/// evaluator selects that operation before coercion, because `CanonPath` has
/// no trailing slash left to inspect. Every path that reaches this callback
/// takes `prim_pathExists`'s other branch, which is `Ancestors`.
///
/// `Ancestors` and not `Full`, and the difference is the whole answer for a
/// dangling symlink: `Full` would resolve the link to its missing target and
/// report `false`, where cppnix leaves the last component alone, `lstat`s the
/// link itself and reports `true`. Resolving nothing at all is wrong the
/// other way -- `maybeLstat` runs `assertNoSymlinks` over the *parent*
/// (`posix-source-accessor.cc:96`), so a path with a symlinked ancestor threw
/// where cppnix answers `true`, and this hook swallowed that into `false`.
/// ENG-12871.
static int rustPathExists(
    void * ctx,
    const unsigned char * root,
    size_t rootLen,
    const unsigned char * path,
    size_t pathLen,
    const unsigned char ** out,
    size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.pathExistsAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.pathExistsAnswer.data());
        *outLen = host.pathExistsAnswer.size();
        return rc;
    };
    try {
        auto st =
            rootedPath(host, root, rootLen, path, pathLen).resolveSymlinks(SymlinkResolution::Ancestors).maybeLstat();
        return answer(st ? "1" : "0", 0);
    } catch (RestrictedPathError &) {
        return answer("0", 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// The string-with-a-trailing-slash arm of `builtins.pathExists`.
///
/// `prim_pathExists` selects this operation before canonicalising the value.
/// Full resolution and the directory check therefore have to remain one host
/// question: the rooted wire path no longer retains the slash that selected
/// the arm. As in cppnix, only a missing path or `RestrictedPathError` is
/// false; a vanished mount and every other failure remain errors.
static int rustDirExists(
    void * ctx,
    const unsigned char * root,
    size_t rootLen,
    const unsigned char * path,
    size_t pathLen,
    const unsigned char ** out,
    size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.dirExistsAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.dirExistsAnswer.data());
        *outLen = host.dirExistsAnswer.size();
        return rc;
    };
    try {
        auto st = rootedPath(host, root, rootLen, path, pathLen).resolveSymlinks(SymlinkResolution::Full).maybeLstat();
        return answer(st && st->type == SourceAccessor::tDirectory ? "1" : "0", 0);
    } catch (RestrictedPathError &) {
        return answer("0", 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// `builtins.readDir`. A transcription of `prim_readDir` (`primops.cc:2508`)
/// from `realisePath` onward, `SymlinkResolution::Full` like
/// `prim_readFile`'s.
///
/// Resolving first also puts the right path in the error text for a symlink
/// to a non-directory: `PosixSourceAccessor::readDirectory` names the path it
/// opened, so cppnix says `cannot read directory ".../foo/git-hates-
/// directories"` where the expression named `.../linked`. That is
/// `eval-fail-readDir-not-a-directory-2`, and it is why skipping the
/// resolution showed up as an error-class mismatch and not only as two
/// refused values.
///
/// One difference, and it is forced by the boundary: `prim_readDir` leaves an
/// entry whose type the filesystem did not report as a thunk that calls
/// `builtins.readFileType` when something forces it. There is no lazy field
/// in a NUL-separated buffer, so the type is resolved here instead. An entry
/// that cannot be stat'ed reads as "unknown" rather than failing the whole
/// listing, because the lazy version would only have failed if the program
/// looked at that one entry, and failing the directory would be stricter than
/// cppnix rather than equal to it.
static int rustReadDir(
    void * ctx,
    const unsigned char * root,
    size_t rootLen,
    const unsigned char * path,
    size_t pathLen,
    const unsigned char ** out,
    size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.readDirAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.readDirAnswer.data());
        *outLen = host.readDirAnswer.size();
        return rc;
    };

    // As in `rustReadFile`.
    try {
        ++host.readDirCalls;
        auto started = std::chrono::steady_clock::now();
        auto dir = rootedPath(host, root, rootLen, path, pathLen).resolveSymlinks(SymlinkResolution::Full);
        auto resolved = std::chrono::steady_clock::now();
        host.readDirResolveNs += std::chrono::duration_cast<std::chrono::nanoseconds>(resolved - started).count();
        std::string encoded;
        for (auto & [name, maybeType] : dir.readDirectory()) {
            auto type = maybeType;
            if (!type) {
                // What the lazy `builtins.readFileType` thunk would have
                // done, eagerly.
                if (auto st = (dir / name).maybeLstat())
                    type = st->type;
            }
            encoded.append(name);
            encoded.push_back('\0');
            encoded.append(fileTypeName(type.value_or(SourceAccessor::tUnknown)));
            encoded.push_back('\0');
        }
        host.readDirListNs +=
            std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now() - resolved).count();
        return answer(encoded, 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// The non-resolving kind query: `maybeLstat`, not `stat`, so a symlink reads
/// as a symlink and a path the accessor has no answer for reads as `absent`.
///
/// A transcription of the accessor call under `prim_readFileType`
/// (`primops.cc:2490`), which is the one read hook that must NOT resolve and
/// the only primop in the family that passes `std::nullopt` to `realisePath`
/// (`primops.cc:2492`). Adding a `resolveSymlinks` here to match its three
/// siblings would be a regression, not a tidy-up: it would answer
/// `"regular"` where cppnix reports the symlink, and it would answer at all
/// on a path with a symlinked ancestor, where cppnix raises
/// `SymlinkNotAllowed` from `maybeLstat`'s `assertNoSymlinks` and so must
/// this. Both halves are pinned by corpus pairs; see
/// `eval-okay-readFileType-symlink` and
/// `eval-fail-readFileType-symlinked-ancestor`.
///
/// # `maybeLstat` and not `lstat`, which is a semantic change and deliberate
///
/// `SourceAccessor::lstat` is `maybeLstat` plus `throw FileNotFound`
/// (`source-accessor.cc:73`), and throwing here made the bridge decide
/// something it has no standing to decide. Two callers ask this question and
/// they want opposite things from a missing path: `builtins.readFileType`
/// wants cppnix's error, and the ancestor scan in `builtins.path`'s filter
/// walk wants what cppnix's `resolveSymlinks` gets from the same accessor
/// call (`source-accessor.cc:91`) -- nullopt, meaning "not a symlink",
/// recorded as the observation `absent`.
///
/// Under pure eval that distinction is the whole ball game. `rootFS` is then
/// a mounted accessor holding `/` -> empty and `/nix/store` -> the store
/// (`eval.cc:294`), so `/nix` -- an ancestor of every store path -- has no
/// mount, falls to the empty accessor and lstats as missing. With the throw
/// here, every filtered `builtins.path` under pure eval died on
/// `path '/nix' does not exist`: 90 of ix's 144 flake attributes, none of
/// which cppnix has any trouble with. ENG-13123.
///
/// So this hands nullopt over as `absent` and the evaluator decides which
/// caller gets an error, which is where the decision belongs.
///
/// `absent` is the only answer that moved. A `RestrictedPathError`, a
/// `SymlinkNotAllowed` or a broken directory is still a non-zero return with
/// its text, because those are the accessor refusing or failing rather than
/// answering, and folding one of them into `absent` would report a forbidden
/// path as an ordinary missing one.
///
/// The half of an `import` that decides whether a path names a directory is
/// `rustFileTypeResolved` below and not this one, because cppnix's `import`
/// resolves where its `readFileType` does not.
static int rustFileType(
    void * ctx,
    const unsigned char * root,
    size_t rootLen,
    const unsigned char * path,
    size_t pathLen,
    const unsigned char ** out,
    size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](std::string_view text, int rc) {
        host.fileTypeAnswer = std::string(text);
        *out = reinterpret_cast<const unsigned char *>(host.fileTypeAnswer.data());
        *outLen = host.fileTypeAnswer.size();
        return rc;
    };

    // As in `rustReadFile`.
    try {
        auto st = rootedPath(host, root, rootLen, path, pathLen).maybeLstat();
        return answer(st ? fileTypeName(st->type) : "absent", 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// The half of an `import` that decides whether a path names a directory and
/// so imports its `default.nix`.
///
/// A transcription of `resolveExprPath` (`eval.cc:3423`), which is where
/// cppnix's `import` does its symlink resolution: `prim_import` passes
/// `std::nullopt` to `realisePath` (`primops.cc:300`) exactly like
/// `prim_readFileType` does, and then `resolveExprPath` resolves anyway. Its
/// directory test is `path.resolveSymlinks().lstat().type == tDirectory`
/// (`eval.cc:3440`), which is `Full` and then the type, so that is what this
/// is.
///
/// Only the directory test is here. `resolveExprPath` also rewrites the path
/// to the symlink's target so a relative import inside the imported file
/// resolves against the target's directory; that is a decision about which
/// path the evaluator then reads, it belongs in the evaluator with the
/// `/default.nix` append it sits beside (`Host::resolve_import`), and this
/// hook answers only the question the world can answer. The two are
/// distinguishable only where the link and its target are different
/// directories with different contents; ENG-12914 tracks it.
///
/// Sharing one hook with `rustFileType` was ENG-12871: `import
/// a/symlinked-dir/f.nix` came back as "path 'a/symlinked-dir' is a symlink"
/// because `lstat`'s `assertNoSymlinks` refuses a symlinked ancestor, which
/// is right for `builtins.readFileType` and wrong for an `import`.
static int rustFileTypeResolved(
    void * ctx,
    const unsigned char * root,
    size_t rootLen,
    const unsigned char * path,
    size_t pathLen,
    const unsigned char ** out,
    size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](std::string_view text, int rc) {
        host.fileTypeResolvedAnswer = std::string(text);
        *out = reinterpret_cast<const unsigned char *>(host.fileTypeResolvedAnswer.data());
        *outLen = host.fileTypeResolvedAnswer.size();
        return rc;
    };

    // As in `rustReadFile`.
    try {
        return answer(
            fileTypeName(
                rootedPath(host, root, rootLen, path, pathLen).resolveSymlinks(SymlinkResolution::Full).lstat().type),
            0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// `builtins.appendContext` makes every key it is handed present before
/// putting it in a string's context (context.cc:270) -- unless read-only mode
/// is on, in which case cppnix skips the call and so does this. That branch
/// lives here rather than in the evaluator because `settings.readOnlyMode` is
/// ours. ENG-12479.
static int
rustEnsurePath(void * ctx, const unsigned char * path, size_t pathLen, const unsigned char ** out, size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto fail = [&](const std::string & text) {
        host.ensurePathAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.ensurePathAnswer.data());
        *outLen = host.ensurePathAnswer.size();
        return 1;
    };

    // As in `rustCopyToStore`: a C++ exception must not unwind through Rust
    // frames, so every failure comes back as a status and a message.
    try {
        std::string p(reinterpret_cast<const char *>(path), pathLen);
        if (!settings.readOnlyMode)
            host.state.store->ensurePath(host.state.store->parseStorePath(p));
        return 0;
    } catch (Error & e) {
        return fail(e.message());
    } catch (std::exception & e) {
        return fail(e.what());
    }
}

/// Import from derivation: make a string context's derivation outputs valid
/// so the evaluator can read through them.
///
/// This calls `EvalState::realiseContext` rather than transcribing it, which
/// is the point of the hook. Every branch that decides what IFD means --
/// `isValidPath` on each element and its program-visible `InvalidPathError`,
/// `allow-import-from-derivation` and its `IFDError`,
/// `trace-import-from-derivation`, `buildPaths`, `resolveDerivedPath`, the
/// `copyClosure` when evaluation and build stores differ, and `allowClosure`
/// on the outputs -- is a `settings` or a store branch that the evaluator
/// cannot see and must not guess at. A second copy of that logic here is
/// exactly the pair of implementations this boundary exists to avoid, and it
/// would drift silently: the two arms would agree on every derivation that
/// builds and disagree only on the ones that do not.
///
/// It also means `readSetTracker->recordStoreQuery` inside `ensureValid`
/// still runs, so a cpp-arm read set covers the same store queries the rust
/// arm records against its own `Question::Realise`.
///
/// `isIFD` is true, not defaulted: every caller on the Rust side is a
/// read-shaped builtin, which is what that flag means. Passing false would
/// quietly disable the `allow-import-from-derivation` check.
/// Decode the NUL-terminated context elements every realise hook receives.
/// Returns nullopt for a malformed request, with the reason in `why`; one
/// copy of the parse, because three hooks now read the same encoding and a
/// drifted copy would make the checked context and the built context
/// different sets.
static std::optional<NixStringContext>
decodeRealiseRequest(const unsigned char * request, size_t requestLen, std::string & why)
{
    NixStringContext context;
    std::string_view rest(reinterpret_cast<const char *>(request), requestLen);
    while (!rest.empty()) {
        auto end = rest.find('\0');
        if (end == std::string_view::npos) {
            why = "rust-eval: unterminated element in realise request";
            return std::nullopt;
        }
        // `parse` and not a hand-rolled split on '!' and '=': the
        // evaluator rendered these with cppnix's own spelling, so they
        // come back through cppnix's own reader.
        context.insert(NixStringContextElem::parse(rest.substr(0, end)));
        rest.remove_prefix(end + 1);
    }
    if (context.empty()) {
        why = "rust-eval: realise request carries no context";
        return std::nullopt;
    }
    return context;
}

/// Phase 1 of a realise: everything `EvalState::realiseContext` does before
/// the build that touches this evaluator's own state. Runs on the evaluation
/// thread -- from the Rust host's `begin`, or from its blocking `realise`,
/// which runs the same three phases in turn -- exactly once per question:
/// the `isValidPath` checks land in `readSetTracker` (a plain map with no
/// lock), and the `allow-import-from-derivation` refusal is decided here,
/// before anything is spawned for it.
///
/// The status is an `IxeRealiseCheck`: build, nothing to build, or failed
/// with the message and its `IxeRealiseErrorClass`. The class is how a
/// refusal keeps the exception type `tryEval` distinguishes (`IFDError`)
/// across the boundary; every other failure is an uncatchable evaluation
/// error, as cppnix's `prim_tryEval` catches `AssertionError` alone.
static int rustRealiseCheck(
    void * ctx,
    const unsigned char * request,
    size_t requestLen,
    int * errorClass,
    const unsigned char ** out,
    size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, IxeRealiseErrorClass error, IxeRealiseCheck status) {
        host.realiseCheckAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.realiseCheckAnswer.data());
        *outLen = host.realiseCheckAnswer.size();
        *errorClass = error;
        return static_cast<int>(status);
    };

    // As in `rustCopyToStore`: a C++ exception must not unwind through Rust
    // frames, so every failure comes back as a status and a message.
    try {
        std::string why;
        auto context = decodeRealiseRequest(request, requestLen, why);
        if (!context)
            return answer(why, IXE_REALISE_ERROR_OTHER, IXE_REALISE_CHECK_FAILED);
        auto drvs = host.state.realiseContextCheck(*context, nullptr, true);
        if (drvs.empty())
            return answer("", IXE_REALISE_ERROR_OTHER, IXE_REALISE_CHECK_NOTHING);
        return answer("", IXE_REALISE_ERROR_OTHER, IXE_REALISE_CHECK_BUILD);
    } catch (IFDError & e) {
        return answer(e.message(), IXE_REALISE_ERROR_IMPORT_FROM_DERIVATION, IXE_REALISE_CHECK_FAILED);
    } catch (Error & e) {
        return answer(e.message(), IXE_REALISE_ERROR_OTHER, IXE_REALISE_CHECK_FAILED);
    } catch (std::exception & e) {
        return answer(e.what(), IXE_REALISE_ERROR_OTHER, IXE_REALISE_CHECK_FAILED);
    }
}

/// Phase 2 runs one batch on the session's build dispatcher. It touches only
/// stores; read tracking and access permissions remain on the evaluator
/// thread. Thread-local result buffers stay separate from the evaluator's
/// hook buffers and from any other session's build dispatcher.
///
/// Success writes the rewrite map, then an empty field as a separator, then
/// the output store paths phase 3 must register in the allow list -- all
/// NUL-terminated, unambiguous because neither a placeholder nor a store
/// path can be empty or contain a NUL.
static int rustRealiseBuild(
    void * ctx, const IxeRealiseRequest * requests, size_t count, IxeRealiseResult * answers)
{
    auto & host = hostOf(ctx);
    static thread_local std::vector<std::string> buffers;

    try {
        buffers.assign(count, "");
        struct Request {
            std::vector<DerivedPath::Built> drvs;
            bool valid = false;
        };
        std::vector<Request> pending(count);
        std::set<DerivedPath::Built> unique;
        for (size_t i = 0; i < count; ++i) {
            answers[i].status = 1;
            try {
                std::string why;
                auto context = decodeRealiseRequest(requests[i].data, requests[i].len, why);
                if (!context)
                    throw Error("%s", why);
                // Assembly only. Phase 1 already checked validity, recorded
                // reads, and enforced allow-import-from-derivation.
                for (auto & c : *context)
                    if (auto * b = std::get_if<NixStringContextElem::Built>(&c.raw))
                        pending[i].drvs.push_back(DerivedPath::Built{
                            .drvPath = b->drvPath,
                            .outputs = OutputsSpec::Names{b->output},
                        });
                if (pending[i].drvs.empty())
                    throw Error("rust-eval: realise build request has nothing to build");
                unique.insert(pending[i].drvs.begin(), pending[i].drvs.end());
                pending[i].valid = true;
            } catch (Error & e) {
                buffers[i] = e.message();
            } catch (std::exception & e) {
                buffers[i] = e.what();
            }
        }

        try {
            std::vector<DerivedPath> buildReqs;
            for (auto & drv : unique)
                buildReqs.emplace_back(drv);
            auto results = host.state.buildStore->buildPathsWithResults(
                buildReqs, bmNormal, host.state.store, BuildFailureMode::KeepGoing);
            std::map<DerivedPath::Built, BuildResult *> byRequest;
            for (auto & result : results)
                if (auto * drv = std::get_if<DerivedPath::Built>(&result.path.raw()))
                    byRequest.emplace(*drv, &result);

            for (size_t i = 0; i < count; ++i) {
                if (!pending[i].valid)
                    continue;
                try {
                    for (auto & drv : pending[i].drvs) {
                        auto result = byRequest.find(drv);
                        if (result == byRequest.end())
                            throw Error("rust-eval: build batch omitted a requested result");
                        result->second->tryThrowBuildError();
                    }
                    StorePathSet outputsToAllow;
                    auto rewrites = host.state.realiseContextOutputs(pending[i].drvs, nullptr, outputsToAllow);
                    std::string encoded;
                    for (auto & [from, to] : rewrites) {
                        encoded.append(from);
                        encoded.push_back('\0');
                        encoded.append(to);
                        encoded.push_back('\0');
                    }
                    encoded.push_back('\0');
                    for (auto & p : outputsToAllow) {
                        encoded.append(host.state.store->printStorePath(p));
                        encoded.push_back('\0');
                    }
                    buffers[i] = std::move(encoded);
                    answers[i].status = 0;
                } catch (Error & e) {
                    buffers[i] = e.message();
                } catch (std::exception & e) {
                    buffers[i] = e.what();
                }
            }
        } catch (Error & e) {
            for (size_t i = 0; i < count; ++i)
                if (pending[i].valid)
                    buffers[i] = e.message();
        } catch (std::exception & e) {
            for (size_t i = 0; i < count; ++i)
                if (pending[i].valid)
                    buffers[i] = e.what();
        }
        for (size_t i = 0; i < count; ++i) {
            answers[i].data = reinterpret_cast<const unsigned char *>(buffers[i].data());
            answers[i].len = buffers[i].size();
        }
        return 0;
    } catch (...) {
        // No exception may unwind through Rust, including an allocation
        // failure while preparing the result buffers.
        return 1;
    }
}

/// Phase 3: register the built outputs in the evaluator's allow list, called
/// from the evaluation thread at the moment the answer is delivered. This is
/// the fix for the one structure the thread-safety audit found unsafe: the
/// allow list behind `EvalState::allowClosure` is a plain prefix set with no
/// lock, read by the evaluation thread on every file access, so the worker
/// must never touch it. Delivery order is the scheduler's token mint order,
/// so when this runs -- and therefore the order the allow list grows in --
/// is a property of the program, not of which build finished first.
///
/// `request` is the output paths from `rustRealiseBuild`'s answer, one
/// NUL-terminated store path per field.
static int rustRealiseAllow(
    void * ctx, const unsigned char * request, size_t requestLen, const unsigned char ** out, size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.realiseAllowAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.realiseAllowAnswer.data());
        *outLen = host.realiseAllowAnswer.size();
        return rc;
    };

    try {
        std::string_view rest(reinterpret_cast<const char *>(request), requestLen);
        while (!rest.empty()) {
            auto end = rest.find('\0');
            if (end == std::string_view::npos)
                return answer("rust-eval: unterminated store path in realise allow request", 1);
            if (end > 0)
                host.state.allowClosure(host.state.store->parseStorePath(rest.substr(0, end)));
            rest.remove_prefix(end + 1);
        }
        return answer("", 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// The evaluator's warnings go to our logger, at our verbosity, formatted the
/// way every other cppnix warning is. Nothing to answer and nothing to fail:
/// a warning below the verbosity threshold is dropped here exactly as one
/// from `warn()` anywhere else in the process would be.
static void rustWarn(void *, const unsigned char * message, size_t messageLen)
{
    warn("%s", std::string_view(reinterpret_cast<const char *>(message), messageLen));
}

/// Split the NUL-separated prefix/path pairs the evaluator sends into
/// cppnix's own LookupPath. The encoding is documented beside
/// `ixe_find_file_fn` in ixe.h.
static LookupPath decodeLookupPath(const unsigned char * entries, size_t entriesLen)
{
    LookupPath lookupPath;
    std::string_view rest(reinterpret_cast<const char *>(entries), entriesLen);
    while (!rest.empty()) {
        auto cut = rest.find('\0');
        if (cut == rest.npos)
            break;
        std::string prefix(rest.substr(0, cut));
        rest.remove_prefix(cut + 1);
        cut = rest.find('\0');
        if (cut == rest.npos)
            break;
        std::string path(rest.substr(0, cut));
        rest.remove_prefix(cut + 1);
        lookupPath.elements.emplace_back(
            LookupPath::Elem{
                .prefix = LookupPath::Prefix{.s = prefix},
                .path = LookupPath::Path{.s = path},
            });
    }
    return lookupPath;
}

/// `<x>` and `builtins.findFile`. The evaluator hands over the list the
/// program actually passed -- which is `__nixPath` unless it rebound it --
/// and cppnix's own findFile does the resolving, because that reaches
/// fetchers, the corepkgs accessor and this evaluator's access control.
/// ENG-12443.
static int rustFindFile(
    void * ctx,
    const unsigned char * entries,
    size_t entriesLen,
    const unsigned char * name,
    size_t nameLen,
    const unsigned char ** out,
    size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.findFileAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.findFileAnswer.data());
        *outLen = host.findFileAnswer.size();
        return rc;
    };

    std::string sought(reinterpret_cast<const char *>(name), nameLen);
    // As in `rustCopyToStore`: a C++ exception must not unwind through Rust
    // frames. The ThrownError case is separated because cppnix raises a miss
    // as one and `builtins.tryEval` catches it, which a corpus case checks.
    try {
        auto path = host.state.findFile(decodeLookupPath(entries, entriesLen), sought);
        if (path.accessor != host.state.rootFS) {
            if (auto mountPoint = host.state.storeFS->findMount(*path.accessor))
                return answer(mountPoint->abs() + std::string("\0", 1) + path.path.abs(), 0);
        }
        // A resolved path is only useful to the evaluator if the evaluator can
        // then read it, and it reads the real filesystem directly. cppnix can
        // resolve into accessors that are not the real one: `corepkgs` holds
        // `<nix/fetchurl.nix>` in memory, and a downloaded search path entry
        // lives behind the accessor its fetcher returned.
        //
        // Handing one of those back as an absolute path alone would name a file
        // that does not exist, which is why this used to refuse (ENG-12443).
        // Instead the bytes go over with it and the evaluator serves the path
        // from memory, so the *path* stays the one cppnix reports and
        // `builtins.toString <nix/fetchurl.nix>` is `/fetchurl.nix` on both
        // arms rather than a store path on one. ENG-12607.
        //
        // Read here rather than lazily because the accessor is the evaluator's
        // and does not outlive it, and because `corepkgs` is one small file: a
        // lazy handle would buy nothing and would have to be kept alive.
        if (path.accessor != host.state.rootFS) {
            auto abs = path.path.abs();
            std::string contents;
            try {
                contents = path.readFile();
            } catch (Error & e) {
                // Resolved but unreadable. Refused rather than reported as a
                // miss: a miss is catchable and would send the caller down a
                // path cppnix never takes.
                return answer(
                    fmt("reading '<%s>' from an accessor that is not the real filesystem: %s", sought, e.message()), 2);
            }
            // The bytes ride in the answer, as its third field: the
            // evaluator holds them for this session and serves every later
            // question about the path from them, the atomic import included.
            return answer(std::string("\0", 1) + abs + std::string("\0", 1) + contents, 0);
        }
        return answer(std::string("\0", 1) + path.path.abs(), 0);
    } catch (ThrownError & e) {
        return answer(e.message(), 5);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// `builtins.nixPath`: the -I flags and NIX_PATH this process was started
/// with, in the same encoding.
static int rustNixPath(void * ctx, const unsigned char ** out, size_t * outLen)
{
    auto & host = hostOf(ctx);
    auto answer = [&](const std::string & text, int rc) {
        host.nixPathAnswer = text;
        *out = reinterpret_cast<const unsigned char *>(host.nixPathAnswer.data());
        *outLen = host.nixPathAnswer.size();
        return rc;
    };

    try {
        std::string encoded;
        for (auto & e : host.state.getLookupPath().elements) {
            encoded.append(e.prefix.s);
            encoded.push_back('\0');
            encoded.append(e.path.s);
            encoded.push_back('\0');
        }
        return answer(encoded, 0);
    } catch (Error & e) {
        return answer(e.message(), 1);
    } catch (std::exception & e) {
        return answer(e.what(), 1);
    }
}

/// Whether the operator has asked this process to stop.
///
/// cppnix's signal handler thread sets the flag; `isInterrupted` is an atomic
/// load, which is what makes it safe to call from inside the VM's poll loop
/// rather than at a scheduler boundary. `checkInterrupt` is the usual
/// spelling and is wrong here: it throws, and a C++ exception must not unwind
/// through Rust frames. ENG-12533.
static int rustInterrupted(void *)
{
    if (isInterrupted())
        return 1;
    return 0;
}

/// `builtins.trace`. cppnix's own wording and its own sink: `printError`
/// with the `trace: ` prefix (`primops.cc:1325`), so the prefix has one copy
/// and it is this one.
static void rustTrace(void *, const unsigned char * message, size_t messageLen)
{
    printError("trace: %1%", std::string_view(reinterpret_cast<const char *>(message), messageLen));
}

/// Build the vtable once, pointing every entry at this object.
///
/// Every field is filled here rather than left for a caller to install, which
/// is the property that replaced fourteen `ixe_set_*` setters: a session
/// cannot be half configured, and a hook cannot arrive after an evaluation
/// has started under the assumption it was absent.
///
/// `rustFileType` and `rustFileTypeResolved` are two different functions on
/// purpose and putting the same one in both fields would be a bug: the first
/// is `builtins.readFileType`, which resolves nothing, and the second is the
/// directory test inside an `import`, which resolves everything. ENG-12871.
///
/// The seven path reads are all present, which is what turns the last seven
/// `Refuse` rows in `rust/nix-eval-rs/src/purity.rs` into served questions,
/// and therefore what lets a flake's files be imported out of a fetched store
/// path under pure eval. The evaluator refuses a partial set outright --
/// `ixe_session_new` returns null and `RustEvalSetup` throws -- because the
/// purity table decides those questions as a group: it can honour `pure-eval`
/// and `restrict-eval` for them only when every one goes through this
/// evaluator's `rootFS`. ENG-12792.
RustEvalHost::RustEvalHost(EvalState & state)
    : state(state)
    , vtable{
          .ctx = this,
          .copy_to_store = rustCopyToStore,
          .store_path = rustStorePath,
          .store_text = rustStoreText,
          .write_derivations = rustWriteDerivations,
          .store_filtered = rustStoreFiltered,
          .fetch = rustFetch,
          .fetch_tree = rustFetchTree,
          .lock_flake = rustLockFlake,
          .parse_flake_ref = rustParseFlakeRef,
          .flake_ref_to_string = rustFlakeRefToString,
          .ensure_path = rustEnsurePath,
          .valid_paths = rustValidPaths,
          .sealed_paths = rustSealedPaths,
          .allow_paths = rustAllowPaths,
          // The threaded import-from-derivation path (ENG-13150). Supplying
          // `realise_build` is this embedder's written consent that the
          // evaluator may call it from a worker thread; the check and allow
          // halves stay on the evaluation thread. All three or none, or the
          // evaluator refuses the vtable.
          .realise_check = rustRealiseCheck,
          .realise_build = rustRealiseBuild,
          .realise_allow = rustRealiseAllow,
          .find_file = rustFindFile,
          .nix_path = rustNixPath,
          .warn = rustWarn,
          .trace = rustTrace,
          .interrupted = rustInterrupted,
          .import_source = rustImport,
          .read_file = rustReadFile,
          .path_exists = rustPathExists,
          .dir_exists = rustDirExists,
          .read_dir = rustReadDir,
          .file_type = rustFileType,
          .file_type_resolved = rustFileTypeResolved,
      }
{
}

/// nix-instantiate.cc's `OutputKind`, which is local to that file. Repeated
/// rather than shared because moving it into the header would put a
/// nix-instantiate detail in front of every other caller of this bridge; the
/// cost is that the two have to be changed together, and the values are
/// checked against it below.
enum OutputKindMirror { okPlain = 0, okRaw = 1, okXML = 2, okJSON = 3 };

/// Raise whatever the Rust side said when it refused a once-only setting.
///
/// These three settings are fixed for the lifetime of the process. Changing
/// one mid-process is an embedder bug, not a user error, and it used to be
/// ignored: an evaluator asked to serve a second store kept the first store's
/// directory and computed every path under it, silently (ENG-12541). Throwing
/// puts it in front of somebody the first time it happens.
static void setOnce(const char * what, int status)
{
    if (status == 0)
        return;
    // Taken and freed here rather than through the IxeString guard, which is
    // declared further down this file; one call, one free, no exit path
    // between them.
    char * raw = ixe_take_setting_conflict();
    std::string text = raw ? std::string(raw) : std::string();
    ixe_string_free(raw);
    if (text.empty())
        text = fmt("%s cannot be changed once the process has set it", what);
    throw Error("rust-eval: %s", text);
}

RustEvalCache::RustEvalCache(uint64_t maxBytes, size_t maxEntries)
    : cache(ixe_eval_cache_new(maxBytes, maxEntries))
{
    if (!cache)
        throw Error("cannot create Rust evaluation cache owner");
}

RustEvalCache::~RustEvalCache()
{
    ixe_eval_cache_free(cache);
}

RustEvalCacheStats RustEvalCache::stats() const
{
    IxeEvalCacheStats result{};
    if (ixe_eval_cache_stats(cache, &result))
        throw Error("cannot read Rust evaluation cache counters");
    return {result.memory_hits, result.disk_loads, result.retained_bytes, result.entries, result.evictions};
}

RustEvalSetup::RustEvalSetup(EvalState & state)
    : perfScope(ixe_perf_reset)
    , hostState(std::make_unique<RustEvalHost>(state))
{
    // Every crossing into nix-eval-rs constructs one of these, so this is the
    // one place that can count what the Rust backend served. The NIX_SHOW_STATS
    // `evaluator` field is derived from this count and the C++ one, rather
    // than echoing the setting that asked for it (ENG-12542).
    state.countRustEval();
    // perfScope resets once per outer question. A nested getFlake enters
    // another setup on this thread and must preserve the work already counted.
    // cppnix bounds recursion with max-call-depth because its evaluator
    // recurses on the host stack. This VM keeps frames on the heap, so the
    // same program allocates instead of faulting and runs until the machine
    // gives out (ENG-12432); the limit has to be handed over explicitly for
    // the two arms to refuse the same programs.
    ixe_set_max_call_depth(state.settings.maxCallDepth);
    // One copy of the version number, ours, handed over rather than
    // duplicated in the Rust crate where it would drift.
    setOnce(
        "nixVersion",
        ixe_set_nix_version(reinterpret_cast<const unsigned char *>(nixVersion.data()), nixVersion.size()));
    // The platform, from the same setting cppnix's own builtins.currentSystem
    // reads, so `--system` moves both arms together.
    {
        const auto & system = state.settings.getCurrentSystem();
        setOnce(
            "currentSystem",
            ixe_set_current_system(reinterpret_cast<const unsigned char *>(system.data()), system.size()));
    }
    // The store directory goes into the fingerprint of every path
    // `builtins.derivationStrict` computes, so the Rust side is told rather
    // than left to assume: an assumed `/nix/store` against a store rooted
    // elsewhere is a wrong output path that looks exactly like a right one.
    {
        const auto & storeDir = state.store->storeDir;
        setOnce(
            "the store directory",
            ixe_set_store_dir(reinterpret_cast<const unsigned char *>(storeDir.data()), storeDir.size()));
    }
    // `~/x` is resolved by the compiler, not by a primop, so the Rust side
    // needs the home directory before it compiles anything. getHome() rather
    // than getenv("HOME") because that is the function the cppnix parser
    // calls (parser.y:467), and its $HOME is validated: unset or unowned
    // falls back to the passwd entry. Two implementations of that rule would
    // differ by naming a different file, so there is one and this hands over
    // its answer.
    //
    // Swallowing the throw is the whole point of the try. cppnix asks this
    // question lazily, from the parser, and only when a `~` literal is
    // actually parsed -- so a process with no `$HOME` and no passwd entry
    // (a container running as a uid nobody created) evaluates everything
    // except home paths perfectly well. Asking eagerly here to hand the
    // answer over would turn that into "every rust-eval evaluation fails",
    // which is a far larger blast radius than the construct deserves.
    // Leaving the setting unmade instead puts the failure back where cppnix
    // has it: the crate has no home directory, so a `~` literal reports
    // `getHomeOf`'s own words and nothing else changes.
    //
    // One divergence survives and is named rather than hidden. With nothing
    // set, the crate falls back to `$HOME` (see `Settings::current`), so the
    // sliver where `$HOME` is set but names a directory this euid does not
    // own AND there is no passwd entry gets that directory here and an error
    // from cppnix. It is left because the fallback is what makes the crate
    // usable standalone, and because the alternative -- handing over an
    // empty string -- resolves `~/x` to `/x`, which is a wrong file rather
    // than no file.
    try {
        const auto home = getHome().string();
        setOnce(
            "the home directory", ixe_set_home_dir(reinterpret_cast<const unsigned char *>(home.data()), home.size()));
    } catch (Error &) {
    }
    // Opt-in on-disk cache of compiled modules and evaluation results. Empty
    // means in-memory only, which is what every release so far has done, so
    // the default flips nothing.
    {
        const auto & dir = state.settings.evalCacheDir.get();
        ixe_set_eval_cache_dir(reinterpret_cast<const unsigned char *>(dir.data()), dir.size());
    }
    // The cache's byte cap, handed over unconditionally for the same reason
    // as the verify rate below: a setter called only when non-zero is a
    // setter whose zero case nobody ever tests. Eviction can only cause a
    // later miss, so this is not in the memo key (SETTER_ACCOUNTING).
    ixe_set_eval_cache_max_bytes(state.settings.evalCacheMaxBytes.get());
    // How often that cache is made to prove itself. Handed over next to the
    // directory because it is meaningless without one, and unconditionally
    // because a setter called only when non-zero is a setter whose zero case
    // nobody ever tests. 0 is the default and evaluates exactly as before.
    //
    // The check exists because a cache is the one component that cannot be
    // checked by reading its output: its output is whatever it was told to
    // say. This is the only call site, so until it existed the crate's
    // `hits_disagreed` counter was structurally zero -- a clean number that
    // meant "never looked" and read as "nothing wrong" (ENG-13092).
    ixe_set_cache_verify_rate(state.settings.evalCacheVerifyRate.get());
    // The two purity settings, separately. They forbid different things, and
    // handing over `restrictEval || pureEval` as one flag -- which is what
    // this replaces -- made the evaluator refuse every host question under
    // either. `rust/nix-eval-rs/src/purity.rs` holds the per-question policy
    // and cites the cppnix line for each row; the hooks installed below are
    // why every row says "serve", since `rustCopyToStore`,
    // `rustStoreFiltered`, `rustFetch`, `rustFetchTree`, `rustFindFile` and
    // the four read hooks all go through this evaluator's own `rootFS`,
    // `checkURI` and `findFile`, so cppnix's access control applies to them
    // unchanged. With no embedder those four reads are absent and the table
    // refuses them instead, which is what the standalone probe and the
    // differential harness see.
    ixe_set_pure_eval(state.settings.pureEval ? 1 : 0);
    ixe_set_restrict_eval(state.settings.restrictEval ? 1 : 0);
    // Rust owns builtin availability. The host supplies only enabled features.
    uint32_t builtinFeatures = 0;
    if (experimentalFeatureSettings.isEnabled(Xp::Flakes))
        builtinFeatures |= IXE_BUILTIN_FLAKES;
    if (experimentalFeatureSettings.isEnabled(Xp::FetchTree))
        builtinFeatures |= IXE_BUILTIN_FETCH_TREE;
    if (experimentalFeatureSettings.isEnabled(Xp::WasmBuiltin))
        builtinFeatures |= IXE_BUILTIN_WASM;
    if (ixe_set_builtin_features(builtinFeatures) != 0)
        throw Error("Rust evaluator rejected builtin feature flags %d", builtinFeatures);
    // The path and URL literal lints live in cppnix's parser, and the Rust
    // compiler now mirrors them at its own literal sites (compile.rs,
    // ENG-12597), so the levels are forwarded rather than refused -- this
    // replaces a requireLintIgnored() that sent every evaluation under a
    // `fatal` lint back by name, including the five eval-okay corpus cases
    // that set `fatal` and then use the form the lint permits.
    //
    // Only `fatal` decides a value: it makes the program illegal, which is
    // meaning and tier 1's business. At `warn` cppnix prints a diagnostic
    // the Rust arm does not; that is warning text, tier 2 (CLAUDE.md,
    // "Parity bar"), where functional equivalence suffices -- the same line
    // this bridge drew when it refused `fatal` and passed `warn` (ENG-12569,
    // measured at nine eval-okay cases lost to refusing `warn`). The level
    // still crosses whole, so the backend knows the setting rather than this
    // bridge deciding what it may hear.
    auto lintLevel = [](const Setting<Diagnose> & setting) {
        switch (setting.get()) {
        case Diagnose::Fatal:
            return 2;
        case Diagnose::Warn:
            return 1;
        case Diagnose::Ignore:
            return 0;
        }
        return 0;
    };
    ixe_set_lint_url_literals(lintLevel(state.settings.lintUrlLiterals));
    ixe_set_lint_short_path_literals(lintLevel(state.settings.lintShortPathLiterals));
    ixe_set_lint_absolute_path_literals(lintLevel(state.settings.lintAbsolutePathLiterals));
    // The store this evaluation copies interpolated paths into. Installed per
    // call rather than once, because the state it answers out of is this
    // call's.
    // The two settings in the trace family that decide values rather than
    // output, so unlike the hooks above they have to be forwarded or the two
    // evaluators answer differently. cppnix picks `prim_trace` or
    // `prim_second` for `builtins.traceVerbose` from the first
    // (`primops.cc:5560`) -- and `prim_second` never forces the message, so
    // `traceVerbose (throw "x") 1` is `1` with it off and dead with it on.
    // The second turns `builtins.warn` into a failure (`primops.cc:1369`).
    // Both are in the Rust evaluator's memo key for the same reason.
    ixe_set_trace_verbose(state.settings.traceVerbose ? 1 : 0);
    ixe_set_abort_on_warn(state.settings.builtinsAbortOnWarn ? 1 : 0);
    // And the experimental feature that decides what `__contentAddressed =
    // true` evaluates to: the feature-is-disabled error with it off, a
    // floating-CA `.drv` with it on (`primops.cc:1632`).
    ixe_set_ca_derivations(experimentalFeatureSettings.isEnabled(Xp::CaDerivations) ? 1 : 0);
    // Policy the memo must not serve across. A witness recorded with
    // import-from-derivation allowed holds realisations the verifier serves
    // by validity without reaching `realiseContextCheck`, which refuses them
    // with it off; one recorded under a wider `allowed-uris` holds fetches
    // served by validity that never reach `checkURI`. Both are in the key.
    // Under `--repair` nothing is served at all: repair means redo.
    ixe_set_allow_import_from_derivation(state.settings.isImportFromDerivationAllowed() ? 1 : 0);
    {
        std::string uris;
        for (auto & uri : state.settings.allowedUris.get()) {
            uris += uri;
            uris.push_back('\n');
        }
        ixe_set_allowed_uris(reinterpret_cast<const unsigned char *>(uris.data()), uris.size());
    }
    ixe_set_repair(state.repair != NoRepair ? 1 : 0);
    // Same gate cppnix applies in `parseHashAlgoOpt` and `Hash::Hash`: Blake3
    // is the feature-is-disabled error without it and a 32-byte hash with it
    // (`libutil/hash.cc:25-29,468-473`).
    ixe_set_blake3_hashes(experimentalFeatureSettings.isEnabled(Xp::BLAKE3Hashes) ? 1 : 0);
    // Same shape for `pipe-operators`, which decides at parse time whether
    // `a |> f` is the feature-is-disabled error or `f a` (lexer.l,
    // parser.y:287-295).
    ixe_set_pipe_operators(experimentalFeatureSettings.isEnabled(Xp::PipeOperators) ? 1 : 0);
    // And for `parse-toml-timestamps`, which decides whether a TOML date in
    // `builtins.fromTOML` is a `{ _type = "timestamp"; }` set or the
    // dates-are-not-supported error (primops.cc, prim_fromTOML).
    ixe_set_parse_toml_timestamps(experimentalFeatureSettings.isEnabled(Xp::ParseTomlTimestamps) ? 1 : 0);
}

const IxeHostVtable * RustEvalSetup::host() const
{
    return &hostState->vtable;
}

RustEvalSetup::~RustEvalSetup()
{
    // Nested host counters belong only to that host. Rust counters include
    // the entire nested call tree and are recorded once, by the outer scope.
    char * line = perfScope.outermost ? ixe_perf_snapshot() : nullptr;
    Finally release([&]() { ixe_string_free(line); });
    EvalPerfCensus::record(
        fmt("%s copyMounted=%d copyAmbient=%d drvWrites=%d drvFlushes=%d readDirCalls=%d readDirResolveNs=%d "
            "readDirListNs=%d",
            line ? line : "",
            hostState->copyMounted,
            hostState->copyAmbient,
            hostState->drvWrites,
            hostState->drvFlushes,
            hostState->readDirCalls,
            hostState->readDirResolveNs,
            hostState->readDirListNs));
}

/// The token the evaluator set for its most recent refusal, or the sentinel.
///
/// `ixe_session_refusal_token` returns null when the last failure was not a
/// refusal, and static storage otherwise -- so this neither frees nor copies.
/// A null becomes `unrecorded` rather than an empty string, because an empty
/// token is a histogram row with no name and the sentinel is a row that says
/// what it is.
static std::string_view refusalTokenOf(IxeSession * session)
{
    if (!session)
        return unrecordedRefusal;
    const char * token = ixe_session_refusal_token(session);
    return token ? std::string_view(token) : unrecordedRefusal;
}

std::shared_ptr<const Pos>
rustEvalPos(EvalState & state, const std::string & source, const char * file, uint32_t line, uint32_t column)
{
    // Line 0 is the evaluator saying it has no position, which is a real
    // answer: an error raised with none of the user's source on the frame
    // stack has nowhere to point, and cppnix prints no `at ...` line for
    // those either. A fabricated 1:1 would be worse than none -- it points at
    // a line that had nothing to do with the failure, and the reader cannot
    // tell it apart from a right one.
    if (line == 0)
        return nullptr;
    Pos::Origin origin = file ? Pos::Origin(state.rootPath(std::string_view(file)))
                              : Pos::Origin(Pos::String{make_ref<std::string>(source)});
    return std::make_shared<Pos>(line, column, origin);
}

void rustEvalThrow(
    EvalState & state, int status, const std::string & message, std::string_view token, std::shared_ptr<const Pos> pos)
{
    switch (status) {
    case 1:
        // `atPos(shared_ptr<const Pos>)` and not `atPos(PosIdx)`: a `PosIdx`
        // names an entry in cppnix's own `PosTable`, and there is none to
        // point at -- nothing in this evaluation went through cppnix's
        // parser, so that table has never seen the file.
        state.error<EvalError>("%s", message).atPos(pos).debugThrow();
    case 2:
        // The evaluator refused, not the command layer, so the token is the
        // one it set rather than a constant from this file. Callers that hold
        // a session pass it; `ixe_eval_expr` has none and therefore no token
        // to give, which is what `unrecorded` is for -- a census that can see
        // how much of its population it cannot classify beats one that
        // attributes it to whatever looked closest.
        RefusalCensus::record(token, message);
        // The class the command layer's `refuse` throws, so a reader that
        // catches refusals by type sees the evaluator's as well.
        throw RustEvalRefusal("rust-eval unimplemented: %s", message);
    case 3:
        state.error<EvalError>("rust-eval parse error: %s", message).debugThrow();
    case 5: {
        // The same class and trace note cppnix's throw primop produces, so a
        // thrown error reads (and classifies) as a throw rather than as an
        // anonymous evaluation failure. Built directly rather than through
        // EvalErrorBuilder: only the templates libexpr instantiates are
        // linkable here, and the variadic addTrace is not one of them.
        ThrownError e(state, ErrorInfo{.level = lvlError, .msg = HintFmt("%s", message), .pos = pos});
        e.addTrace(nullptr, HintFmt("while calling the '%s' builtin", "throw"));
        throw e;
    }
    case 6:
        throw AssertionError(state, ErrorInfo{.level = lvlError, .msg = HintFmt("%s", message), .pos = pos});
    case 7:
        // Only reachable if a caller forgets to handle a missing attribute
        // itself, which it should: the message is a bare name, and the
        // sentence it belongs in depends on what was being selected.
        throw Error("rust-eval: '%s' not found", message);
    case IXE_ERR_IFD:
        state.error<IFDError>("%s", message).atPos(pos).debugThrow();
    case IXE_ERR_MISSING_ARGUMENT:
        state.error<MissingArgumentError>("%s", message).atPos(pos).debugThrow();
    case IXE_ERR_ATTR_PATH_NOT_FOUND:
        throw AttrPathNotFound("%s", message);
    default:
        // Status 4 lands here, which is the point. It used to `break` out of
        // the switch in rust-eval.cc and return normally, so a bad call into
        // nix-eval-rs printed nothing and exited 0.
        throw Error("rust-eval: invalid call into nix-eval-rs (status %d): %s", status, message);
    }
}

namespace {

/// Owns a string the C ABI handed back, so every exit path frees it.
struct IxeString
{
    char * s = nullptr;

    IxeString() = default;
    IxeString(const IxeString &) = delete;
    IxeString & operator=(const IxeString &) = delete;

    ~IxeString()
    {
        ixe_string_free(s);
    }

    std::string str() const
    {
        return s ? std::string(s) : std::string();
    }
};

/// Owns a NUL-delimited buffer from `ixe_attrs_names` or
/// `ixe_get_string_context`. Distinct from `IxeString` because it needs its
/// length both to be walked and to be freed.
struct IxeNames
{
    char * p = nullptr;
    size_t len = 0;

    IxeNames() = default;
    IxeNames(const IxeNames &) = delete;
    IxeNames & operator=(const IxeNames &) = delete;

    ~IxeNames()
    {
        ixe_names_free(p, len);
    }

    /// The names as a set, which is the shape `Suggestions::bestMatches`
    /// takes and the shape cppnix's own miss branch builds.
    StringSet set() const
    {
        StringSet names;
        for (size_t i = 0; i < len;) {
            size_t n = strnlen(p + i, len - i);
            names.emplace(p + i, n);
            i += n + 1;
        }
        return names;
    }
};

/// Owns a session and everything in its handle table.
struct RustError
{
    std::string message;
    /// Where in the user's source it happened, or null when nowhere.
    std::shared_ptr<const Pos> pos;
};

struct IxeSessionRef
{
    IxeSession * p;

    /// `setup` must outlive this session: the vtable is copied, but the
    /// context object and the answer buffers it points at belong to the
    /// setup. Every caller here keeps both on one stack frame, the setup
    /// declared first.
    ///
    /// `p` is null when the evaluator refused the host, which today means a
    /// partial set of the seven path reads; the callers check it and throw.
    explicit IxeSessionRef(const RustEvalSetup & setup, const RustEvalCache * cache = nullptr)
        : p(ixe_session_new(setup.host()))
    {
        if (p && cache && ixe_session_set_eval_cache(p, cache->get())) {
            ixe_session_free(p);
            p = nullptr;
            throw Error("cannot attach Rust evaluation cache owner");
        }
        if (!p) {
            IxeString diagnostic;
            diagnostic.s = ixe_take_setting_conflict();
            if (diagnostic.s)
                throw Error("Rust evaluator: %s", diagnostic.str());
        }
    }

    /// The text this session is evaluating.
    ///
    /// Kept because a position whose origin is a string has to carry the
    /// string: cppnix renders `at «string»:L:C` with the expression quoted
    /// underneath, and it reads those lines out of the origin rather than off
    /// disk. `askQuestion` sets it, which is the one place a session is given
    /// something to evaluate.
    std::string source;

    IxeSessionRef(const IxeSessionRef &) = delete;
    IxeSessionRef & operator=(const IxeSessionRef &) = delete;

    ~IxeSessionRef()
    {
        ixe_session_free(p);
    }

    /// What the last non-zero status left behind: the message, and where in
    /// the user's source it happened.
    ///
    /// One call and one value rather than a message here and a position
    /// there, because the ABI hands them over together for a reason: two
    /// accessors could be called in either order, and the order that asks for
    /// the position after taking the message gets nothing, silently.
    ///
    /// An empty message is itself a bug in the Rust side rather than a
    /// silence to paper over, so the caller gets a placeholder it can grep
    /// for.
    RustError takeError(EvalState & state) const
    {
        IxeString message;
        IxePos at = {nullptr, 0, 0};
        message.s = ixe_session_take_error(p, &at);
        auto pos = rustEvalPos(state, source, at.file, at.line, at.column);
        ixe_string_free(at.file);
        return RustError{message.s ? message.str() : "(no message)", pos};
    }

    /// Send whatever the evaluator wants to say about a damaged cache entry
    /// to stderr. Not into the returned value: the value is the expression's
    /// answer, and a cache complaint is not part of it.
    void drainWarnings() const
    {
        while (true) {
            IxeString warning;
            warning.s = ixe_session_take_warning(p);
            if (!warning.s)
                return;
            std::cerr << "rust-eval: warning: " << warning.str() << "\n";
        }
    }

    [[noreturn]] void fail(EvalState & state, int status) const
    {
        auto failure = takeError(state);
        rustEvalThrow(state, status, failure.message, refusalTokenOf(p), failure.pos);
    }
};

/// Describe the evaluand's arguments in the ABI's terms.
///
/// A view over `evaluand.args`, not a copy: `IxeArgument` holds a pointer, so
/// the evaluand has to outlive the call this is passed to. Every caller here
/// keeps it on the stack for the whole session.
///
/// The bridge no longer *builds* these values. It used to, with
/// `ixe_alloc_json` and `ixe_internal_primop`, and applied them with
/// `ixe_apply` after the root came back -- which is why an evaluand with
/// arguments could not be memoised at all: the memo key knew about the
/// source and knew nothing about what the bridge had applied to it, so two
/// flakes were one key. Handing the list to `ixe_session_eval_question`
/// instead makes one list both the key and the value, and the ABI refuses the
/// three building calls while a question is in flight so that nobody can
/// reintroduce the gap. ENG-12915.
static std::vector<IxeArgument> rustArgumentViews(const RustEvaluand & evaluand)
{
    std::vector<IxeArgument> views;
    views.reserve(evaluand.args.size());
    for (auto & argument : evaluand.args)
        views.push_back(
            IxeArgument{
                .kind = argument.kind == RustArgument::Kind::Json ? IXE_ARG_JSON : IXE_ARG_INTERNAL_PRIMOP,
                .text =
                    IxeBytes{
                        .text = reinterpret_cast<const unsigned char *>(argument.text.data()),
                        .len = argument.text.size(),
                    },
            });
    return views;
}

/// The candidate attribute paths, in the ABI's terms. A view, as above.
/// Views over `evaluand.autoArgs`, held for the length of the question call.
static std::vector<IxeAutoArg> rustAutoArgViews(const RustEvaluand & evaluand)
{
    std::vector<IxeAutoArg> views;
    views.reserve(evaluand.autoArgs.size());
    for (auto & arg : evaluand.autoArgs)
        views.push_back(
            IxeAutoArg{
                .name =
                    IxeBytes{.text = reinterpret_cast<const unsigned char *>(arg.name.data()), .len = arg.name.size()},
                .kind = arg.kind == RustAutoArg::Kind::Expr ? IXE_AUTO_ARG_EXPR : IXE_AUTO_ARG_STRING,
                .text =
                    IxeBytes{.text = reinterpret_cast<const unsigned char *>(arg.text.data()), .len = arg.text.size()},
            });
    return views;
}

static std::vector<IxeBytes> rustAttrPathViews(const RustEvaluand & evaluand)
{
    std::vector<IxeBytes> views;
    views.reserve(evaluand.attrPaths.size());
    for (auto & path : evaluand.attrPaths)
        views.push_back(
            IxeBytes{
                .text = reinterpret_cast<const unsigned char *>(path.data()),
                .len = path.size(),
            });
    return views;
}

/// Owns one handle. Frees on scope exit, so an exception thrown mid-walk
/// does not leak the handles the walk had opened.
struct IxeValue
{
    IxeSession * session = nullptr;
    IxeHandle handle = 0;

    IxeValue() = default;

    IxeValue(IxeValue && other) noexcept
        : session(other.session)
        , handle(other.handle)
    {
        other.session = nullptr;
        other.handle = 0;
    }

    IxeValue(const IxeValue &) = delete;
    IxeValue & operator=(const IxeValue &) = delete;

    IxeValue & operator=(IxeValue && other) noexcept
    {
        if (this == &other)
            return *this;
        reset(nullptr, 0);
        session = other.session;
        handle = other.handle;
        other.session = nullptr;
        other.handle = 0;
        return *this;
    }

    ~IxeValue()
    {
        if (session && handle)
            ixe_handle_free(session, handle);
    }

    void reset(IxeSession * s, IxeHandle h)
    {
        if (session && handle)
            ixe_handle_free(session, handle);
        session = s;
        handle = h;
    }

    /// Give up ownership and return the handle. For the one place a handle
    /// changes owner: an empty attribute path selects the value the source
    /// produced, so the walk's result *is* the root, and two `IxeValue`s
    /// holding it would free it twice.
    IxeHandle release()
    {
        auto released = handle;
        session = nullptr;
        handle = 0;
        return released;
    }
};

int renderMode(RustRender render)
{
    switch (render) {
    case RustRender::Json:
        return IXE_RENDER_JSON;
    case RustRender::Raw:
        return IXE_RENDER_RAW;
    case RustRender::ValuePrinter:
        return IXE_RENDER_VALUE_PRINTER;
    case RustRender::Xml:
        return IXE_RENDER_XML;
    case RustRender::PlainLazy:
        return IXE_RENDER_PLAIN_LAZY;
    case RustRender::Plain:
        return IXE_RENDER_PLAIN;
    }
    unreachable();
}

} // namespace

namespace {

/// What `askQuestion` came back with, in the caller's terms.
///
/// Three outcomes, named, because two of them carry an answer and a caller
/// distinguishing them by which pointer is null would have to get a two-way
/// test right. The one it would get wrong quietly is Verify, whose failure
/// mode is skipping the check nothing else performs.
enum class Served {
    /// The cache answered. `answer` is it; do no work.
    Answer,
    /// Nothing was cached. `root` is the expression; do the work and report it.
    Evaluate,
    /// A sampled check of a cached answer. Do the work, report it, and return
    /// `answer` -- the cached one -- so the command's output does not depend
    /// on whether the sampler picked this run.
    Verify,
};

/// Ask nix-eval-rs one whole question, and find out whether it is already
/// answered.
///
/// The whole question, not just the source: which attribute path and which
/// bytes at the end of it are as much a part of what is being asked as the
/// expression is, and a memo key that leaves them out can only serve the one
/// caller whose question never varies. That is why `eval-cache-dir` wrote
/// objects for `nix eval` and `nix build` and served neither of them until
/// ENG-12830.
Served askQuestion(
    EvalState & state,
    IxeSessionRef & session,
    IxeValue & root,
    std::string & answer,
    const RustEvaluand & evaluand,
    int kind,
    int render)
{
    int mode = IXE_SERVE_EVALUATE;
    IxeHandle handle = 0;
    IxeString served;
    auto & source = evaluand.src.source;
    session.source = source;
    auto & baseDir = evaluand.src.baseDir;
    auto & file = evaluand.src.file;
    auto & sourceRoot = evaluand.src.root;
    if (int rc = ixe_session_set_source_root(
            session.p, reinterpret_cast<const unsigned char *>(sourceRoot.data()), sourceRoot.size());
        rc != 0)
        session.fail(state, rc);
    // Views into `evaluand`, which the caller holds for the whole session.
    auto args = rustArgumentViews(evaluand);
    auto attrPaths = rustAttrPathViews(evaluand);
    auto autoArgs = rustAutoArgViews(evaluand);
    int rc = ixe_session_eval_question(
        session.p,
        reinterpret_cast<const unsigned char *>(source.data()),
        source.size(),
        reinterpret_cast<const unsigned char *>(baseDir.data()),
        baseDir.size(),
        // Empty means no file, which the Rust side reads as a string origin:
        // `data()` on an empty std::string is non-null, so the length is what
        // carries the distinction.
        file.empty() ? nullptr : reinterpret_cast<const unsigned char *>(file.data()),
        file.size(),
        args.empty() ? nullptr : args.data(),
        args.size(),
        kind,
        attrPaths.empty() ? nullptr : attrPaths.data(),
        attrPaths.size(),
        evaluand.indexLists ? 1 : 0,
        evaluand.apply ? reinterpret_cast<const unsigned char *>(evaluand.apply->data()) : nullptr,
        evaluand.apply ? evaluand.apply->size() : 0,
        autoArgs.empty() ? nullptr : autoArgs.data(),
        autoArgs.size(),
        evaluand.autoCall ? 1 : 0,
        render,
        &mode,
        &handle,
        &served.s);
    session.drainWarnings();
    if (rc != 0)
        session.fail(state, rc);
    answer = served.str();
    if (handle)
        root.reset(session.p, handle);
    switch (mode) {
    case IXE_SERVE_ANSWER:
        return Served::Answer;
    case IXE_SERVE_VERIFY:
        return Served::Verify;
    case IXE_SERVE_EVALUATE:
        return Served::Evaluate;
    default:
        /* A mode this build does not know is the two sides disagreeing about
           the protocol, which is exactly when guessing is worst: guessing
           Answer returns an empty string as the value, and guessing Evaluate
           silently drops a verification. */
        throw Error("rust-eval: unknown serve mode %1% from the evaluation cache", mode);
    }
}

/// Tell nix-eval-rs what the walk produced, so the next process can skip it.
///
/// Unconditional at every call site, including the ones that were served: it
/// does nothing when no question is in flight, and one line that is always
/// there cannot be forgotten in the branch nobody tested.
///
/// There is deliberately no failure counterpart. Any throw between the two
/// calls leaves the question unfiled, because `~IxeSessionRef` drops the
/// scope and only this call records. That is the behaviour we want and not an
/// oversight: a failure on this path can be raised by this bridge rather than
/// by the evaluator -- a missing attribute carrying the sibling names it
/// suggests, a refusal carrying a token -- and none of those survive a round
/// trip through the (status, text) pair a memo row holds. ENG-12857.
void reportAnswer(EvalState & state, IxeSessionRef & session, const std::string & answer)
{
    int rc = ixe_session_question_answer(
        session.p, 0, reinterpret_cast<const unsigned char *>(answer.data()), answer.size());
    session.drainWarnings();
    // An answer the cache would not take is a failure to file, not a filed
    // answer: proceeding as if it were would report a memo that never
    // happened.
    if (rc != 0)
        session.fail(state, rc);
}

/// Tell nix-eval-rs the answer it served could not be used, so it forgets the
/// row and its witness and the question evaluates when asked again.
void rejectAnswer(EvalState & state, IxeSessionRef & session, const std::string & why)
{
    if (int rc =
            ixe_session_question_reject(session.p, reinterpret_cast<const unsigned char *>(why.data()), why.size());
        rc != 0)
        session.fail(state, rc);
    session.drainWarnings();
}

/// One question a command asks the evaluator: served from the memo, or
/// evaluated, and either way filed.
///
/// The commands that ask one (select, build, develop, run, flake show) each
/// carried this skeleton -- create the session, ask, decode a served answer,
/// evaluate otherwise, file the answer, hand back the served one under a
/// sampled check -- and none of them handled a served answer that would not
/// decode: a damaged or stale row was dereferenced as if fresh. Here it is
/// once. A decoder that throws makes the cache forget the row (`rejectAnswer`)
/// and this run evaluate, so a corrupt answer is a miss and never a value.
template<typename T>
struct RustQuestion
{
    using Decode = std::function<T(const std::string &)>;
    using Encode = std::function<std::string(const T &)>;

    EvalState & state;
    const RustEvaluand & evaluand;
    int kind;
    int render;
    RustEvalSetup setup;
    IxeSessionRef session;
    IxeValue root;
    std::string served;
    Served mode = Served::Evaluate;

    RustQuestion(
        EvalState & state, const RustEvaluand & evaluand, int kind, int render, const RustEvalCache * cache = nullptr)
        : state(state)
        , evaluand(evaluand)
        , kind(kind)
        , render(render)
        , setup(state)
        , session(setup, cache)
    {
        if (!session.p) {
            /* The evaluator refused the host it was handed. The one way that
               happens today is a partial set of the seven path reads, which
               would leave this evaluator reading outside the process's access
               control; the reason comes back through the setting-conflict slot
               rather than being guessed at here. */
            IxeString why;
            why.s = ixe_take_setting_conflict();
            throw Error(
                "rust-eval: could not create an evaluation session: %s",
                why.s ? why.str() : "the evaluator gave no reason");
        }
        mode = askQuestion(state, session, root, served, evaluand, kind, render);
    }

    /// The served answer, decoded; nothing when this run evaluates.
    std::optional<T> answered(const Decode & decode)
    {
        if (mode != Served::Answer)
            return std::nullopt;
        try {
            return decode(served);
        } catch (std::exception & e) {
            rejectAnswer(state, session, e.what());
            mode = askQuestion(state, session, root, served, evaluand, kind, render);
            if (mode == Served::Answer)
                throw Error("rust-eval: the evaluation cache served an answer it was just told to forget");
            return std::nullopt;
        }
    }

    /// File what the walk produced, and hand back what the caller should use:
    /// under a sampled check the SERVED answer, for the reason `rustEvalRender`
    /// gives -- what gets built must not depend on whether the verifier picked
    /// this run, and a disagreement is an error-priority complaint out of the
    /// evaluator, which is the part that must be seen; otherwise the fresh one.
    T settle(T found, const Encode & encode, const Decode & decode)
    {
        reportAnswer(state, session, encode(found));
        if (mode != Served::Verify)
            return found;
        try {
            return decode(served);
        } catch (std::exception & e) {
            rejectAnswer(state, session, e.what());
            return found;
        }
    }
};

/// What the Rust evaluator reads for `flake::readFlake`: the file's own text,
/// read through its mounted input on every pass. A symlinked file resolves
/// literals beside its target; the document keeps the original flake
/// directory so its consumer resolves paths in that same directory. The
/// root reaches Rust's module identity and every host request, including
/// reads made while evaluating dynamic names or rendering publicKeys.
static RustEvaluand flakeDocumentEvaluand(EvalState & state, const SourcePath & flakeNix)
{
    auto resolved = resolveExprPath(flakeNix);
    auto [storePath, subPath] = state.store->toStorePath(flakeNix.path.abs());
    auto root = state.store->printStorePath(storePath);
    if (!state.storeFS->getMount(CanonPath(root)))
        throw Error("rust-eval: flake document input '%s' is not mounted", root);
    return RustEvaluand{
        .src =
            RustSource{
                .source = resolved.readFile(),
                .baseDir = resolved.parent().path.abs(),
                .file = flakeNix.path.abs(),
                .root = root,
            },
        // The whole file. `decode_attr_paths` refuses an empty list, so the
        // one path is the empty one.
        .attrPaths = {""},
        .indexLists = false,
    };
}

/// `flake::readFlake`'s Rust arm: `flake.nix` as a document, one question per
/// file, memoised like every other question. libflake declares the seam
/// (`flake-document.hh`) and this file, which links the evaluator, installs
/// both halves at load (`RegisterRustFlakeDocument` below).
static nlohmann::json rustFlakeDocument(EvalState & state, const RustEvaluand & evaluand)
{
    RustQuestion<nlohmann::json> question(state, evaluand, IXE_QUESTION_FLAKE_DOCUMENT, IXE_RENDER_RAW);
    auto decode = [](const std::string & text) { return nlohmann::json::parse(text); };
    auto encode = [](const nlohmann::json & document) { return document.dump(); };
    if (auto served = question.answered(decode))
        return *served;
    IxeString out;
    if (int rc = ixe_flake_document(question.session.p, question.root.handle, &out.s); rc != 0)
        question.session.fail(state, rc);
    question.session.drainWarnings();
    return question.settle(decode(out.str()), encode, decode);
}

static const struct RegisterRustFlakeDocument
{
    RegisterRustFlakeDocument()
    {
        flake::flakeDocumentReader() = [](EvalState & state, const SourcePath & flakeNix) {
            return rustFlakeDocument(state, flakeDocumentEvaluand(state, flakeNix));
        };
    }
} registerRustFlakeDocument;

/// cppnix's `showType` for a handle's `IXE_TYPE_*`: the words its messages
/// use for a value of that type.
static std::string_view ixeTypeName(int type)
{
    switch (type) {
    case IXE_TYPE_INT:
        return "an integer";
    case IXE_TYPE_FLOAT:
        return "a float";
    case IXE_TYPE_BOOL:
        return "a Boolean";
    case IXE_TYPE_NULL:
        return "null";
    case IXE_TYPE_STRING:
        return "a string";
    case IXE_TYPE_PATH:
        return "a path";
    case IXE_TYPE_LIST:
        return "a list";
    case IXE_TYPE_ATTRS:
        return "a set";
    case IXE_TYPE_FUNCTION:
        return "a function";
    default:
        return "an unknown value";
    }
}

/// Select using the same retained Rust question that owns the cache key.
std::string selectFrom(EvalState & state, IxeSessionRef & session, IxeValue & current)
{
    IxeHandle selected = 0;
    IxeString path;
    if (int rc = ixe_question_select(session.p, current.handle, &selected, &path.s); rc != 0)
        session.fail(state, rc);
    current.reset(session.p, selected);
    return path.str();
}

} // namespace

static std::string rustEvalSelection(
    EvalState & state,
    const RustEvaluand & evaluand,
    int render,
    bool nestedFailureIsUnimplemented,
    const RustEvalCache * cache = nullptr)
{
    /* One path, whether or not the evaluand has arguments. It used to be two,
       because a flake's arguments were applied by this bridge after the fact
       and appeared in no memo key, so `mayBeMemoised` sent every flake down a
       branch that could not be served. The arguments now cross on this call
       and are keyed on, which is ENG-12915 and is what gives `nix eval
       <flake>#attr` warm starts. The answer is the rendered text itself, so
       both codecs are the identity. */
    RustQuestion<std::string> question(state, evaluand, IXE_QUESTION_SELECT, render, cache);
    auto identity = [](const std::string & text) { return text; };
    if (auto answer = question.answered(identity))
        return std::move(*answer);
    auto & session = question.session;
    auto & current = question.root;

    IxeString out;
    int rc = ixe_eval_command_execute(session.p, current.handle, &out.s);
    session.drainWarnings();
    if (rc != 0) {
        auto failure = session.takeError(state);
        if (nestedFailureIsUnimplemented && rc != 2 /* already unimplemented */)
            refuse(
                refusalTokens::unsupported,
                "printing a value that fails below the top level "
                "(cppnix prints «error: %s» in place and carries on)",
                failure.message);
        rustEvalThrow(state, rc, failure.message, refusalTokenOf(session.p), failure.pos);
    }
    return question.settle(out.str(), identity, identity);
}

std::string
rustEvalRender(EvalState & state, const RustEvaluand & evaluand, RustRender render, bool nestedFailureIsUnimplemented)
{
    return rustEvalSelection(state, evaluand, renderMode(render), nestedFailureIsUnimplemented);
}

static unsigned int evalCommandFlags(const RustEvalCommandOptions & options)
{
    return (options.raw ? IXE_EVAL_RAW : 0u) | (options.json ? IXE_EVAL_JSON : 0u)
           | (options.pretty ? IXE_EVAL_PRETTY : 0u) | (options.file ? IXE_EVAL_FILE : 0u)
           | (options.expr ? IXE_EVAL_EXPR : 0u) | (options.writeTo ? IXE_EVAL_WRITE_TO : 0u);
}

static int validateEvalCommand(const RustEvalCommandOptions & options)
{
    int render = 0;
    IxeString error;
    if (ixe_eval_command_validate(
            evalCommandFlags(options),
            reinterpret_cast<const unsigned char *>(options.installable.data()),
            options.installable.size(),
            &render,
            &error.s))
        throw UsageError("%s", error.str());
    return render;
}

void rustValidateEvalCommand(const RustEvalCommandOptions & options)
{
    validateEvalCommand(options);
}

std::string rustEvalCommand(
    EvalState & state,
    const RustEvaluand & evaluand,
    const RustEvalCommandOptions & options,
    const RustEvalCache * cache)
{
    auto canonical = rustEvalSelection(state, evaluand, validateEvalCommand(options), false, cache);
    IxeString output, error;
    int appendNewline = 0;
    if (ixe_eval_command_format(
            evalCommandFlags(options),
            reinterpret_cast<const unsigned char *>(canonical.data()),
            canonical.size(),
            &output.s,
            &appendNewline,
            &error.s))
        throw Error("%s", error.str());
    if (output.s)
        canonical = output.str();
    if (appendNewline)
        canonical.push_back('\n');
    return canonical;
}

/// Hand the refusal vocabulary to the census, once, at load.
///
/// The census lives in libexpr and the vocabulary is the evaluator's, defined
/// in `rust/nix-eval-rs/src/refusal.rs` and enumerable over the C ABI that
/// only this library links. Registering it rather than restating it is what
/// keeps there being one list: a second copy in libexpr would drift the first
/// time either side gained a token, and the histogram's denominator would
/// quietly stop covering it.
static const struct RegisterRefusalVocabulary
{
    RegisterRefusalVocabulary()
    {
        std::vector<std::string> tokens;
        auto count = ixe_refusal_token_count();
        tokens.reserve(count);
        for (size_t i = 0; i < count; i++)
            if (const char * name = ixe_refusal_token_at(i))
                tokens.emplace_back(name);
        RefusalCensus::setVocabulary(std::move(tokens));
    }
} registerRefusalVocabulary;

namespace {

/// Select `name` off an attribute set handle, or report that it is absent.
///
/// Absence is a normal answer here, not a failure: `outputs`, `meta` and
/// `outputSpecified` are all optional on a derivation and each has its own
/// default. Anything other than absence is the session's failure to raise.
bool selectOptional(
    EvalState & state, IxeSessionRef & session, IxeHandle attrs, const std::string & name, IxeValue & out)
{
    IxeHandle next = 0;
    int rc =
        ixe_attrs_select(session.p, attrs, reinterpret_cast<const unsigned char *>(name.data()), name.size(), &next);
    if (rc == IXE_ERR_MISSING) {
        (void) session.takeError(state);
        return false;
    }
    if (rc != 0)
        session.fail(state, rc);
    out.reset(session.p, next);
    return true;
}

/// Force a handle and read it as a string with its context. A value of
/// another type is a REFUSAL under `token`, not an error of this walk's own:
/// cppnix's diagnostic for that shape is the canonical one, and a refusal
/// is what hands the command back to it, while an error here would be a
/// second text for the same fault (the comparator counts that as a
/// divergence). Each walk names the token it refuses under -- the
/// derivation walks `notADerivation`, the app walk `notAnApp` -- so the
/// census files the refusal against the shape that was actually read.
///
/// `where` names the attribute for both the refusal and the census, so a
/// derivation with, say, a list where cppnix wants a string is one histogram
/// row that says which attribute rather than a shrug.
std::pair<std::string, NixStringContext> forceStringWithContext(
    EvalState & state, IxeSessionRef & session, IxeValue & value, const std::string & where, std::string_view token)
{
    if (int rc = ixe_force(session.p, value.handle); rc != 0)
        session.fail(state, rc);
    auto type = ixe_value_type(session.p, value.handle);
    if (type != IXE_TYPE_STRING && type != IXE_TYPE_PATH)
        refuse(token, "%s is not a string", where);

    IxeString text;
    auto rc = type == IXE_TYPE_PATH ? ixe_get_string(session.p, value.handle, &text.s)
                                    : ixe_render(session.p, value.handle, IXE_RENDER_RAW, &text.s);
    if (rc != 0)
        session.fail(state, rc);
    if (type == IXE_TYPE_PATH)
        return {text.str(), {}};
    IxeNames encoded;
    if (int rc = ixe_get_string_context(session.p, value.handle, &encoded.p, &encoded.len); rc != 0)
        session.fail(state, rc);

    NixStringContext context;
    for (size_t i = 0; i < encoded.len;) {
        auto len = strnlen(encoded.p + i, encoded.len - i);
        context.insert(NixStringContextElem::parse(std::string_view(encoded.p + i, len)));
        i += len + 1;
    }
    return {text.str(), std::move(context)};
}

/// The text alone; see `forceStringWithContext`.
std::string forceString(
    EvalState & state, IxeSessionRef & session, IxeValue & value, const std::string & where, std::string_view token)
{
    return forceStringWithContext(state, session, value, where, token).first;
}

std::string encodeApp(Store & store, const UnresolvedApp & app)
{
    auto context = nlohmann::json::array();
    for (auto & path : app.unresolved.context)
        context.push_back(path.to_string(store));
    return nlohmann::json{{"program", app.unresolved.program.string()}, {"context", std::move(context)}}.dump();
}

UnresolvedApp decodeApp(Store & store, const std::string & encoded)
{
    auto json = nlohmann::json::parse(encoded);
    std::vector<DerivedPath> context;
    for (auto & path : json.at("context"))
        context.push_back(DerivedPath::parse(store, path.get<std::string>()));
    return UnresolvedApp{App{
        .context = std::move(context),
        .program = json.at("program").get<std::string>(),
    }};
}

} // namespace

namespace {

/// A derivation as a memo row's answer: the drvPath, then one output name per
/// line.
///
/// A format of its own rather than a reuse of an existing renderer. What `nix
/// build` wants out of an evaluation is not printable bytes -- it is a store
/// path and a set of output names -- so there is no render mode whose output
/// would do. Newline-separated is unambiguous because neither a store path
/// nor an output name can contain one.
std::string encodeDerivation(EvalState & state, const RustDerivation & found)
{
    std::string out = state.store->printStorePath(found.drvPath);
    for (auto & name : found.outputs)
        out += "\n" + name;
    return out;
}

/// Root a derivation a served answer names, as the write that produced it
/// did on the recording run (`Store::writeDerivation` takes a temporary root
/// before its validity check). The verifier checked the witness's
/// derivations are still in the store without rooting each of them
/// (`ixe_valid_paths_fn`); the closure of an answered derivation covers its
/// input derivations, so rooting the answers is what a build needs.
void rootServedDerivation(EvalState & state, const StorePath & drvPath)
{
    state.store->addTempRoot(drvPath);
}

/// The inverse, with the checks the fresh path applies applied again.
///
/// `requireDerivation` a second time is not belt and braces. A served answer
/// is bytes off disk that nothing in this process computed, and a build
/// pointed at a path that is not a derivation is precisely the failure the
/// memo-hit ratchet exists to prevent.
RustDerivation decodeDerivation(EvalState & state, const std::string & encoded)
{
    auto lines = tokenizeString<std::vector<std::string>>(encoded, "\n");
    if (lines.empty())
        throw Error("rust-eval: the evaluation cache holds a derivation answer with no drvPath");
    auto drvPath = state.store->parseStorePath(lines.front());
    drvPath.requireDerivation();
    StringSet outputs(lines.begin() + 1, lines.end());
    /* cppnix never produces an empty output set -- `out` is the default and
       `meta.outputsToInstall` reducing to nothing is refused on the fresh
       path -- so an empty one here is a damaged row, and a build of no
       outputs is the shape nobody notices. */
    if (outputs.empty())
        throw Error("rust-eval: the evaluation cache holds a derivation answer with no outputs");
    return RustDerivation{.drvPath = std::move(drvPath), .outputs = std::move(outputs)};
}

} // namespace

/// The `overrides` argument of `call-flake.nix`, as JSON.
///
/// cppnix's `callFlake` (`flake.cc:1075`) builds this as a `Value`; this
/// builds the same thing as a document `ixe_alloc_json` decodes, because a
/// `Value` cannot cross the ABI and a lock file can.
///
/// Two things are deliberate and both were learned from `rustFetchTree`
/// above. The set is serialised attribute by attribute, never as a unit,
/// because `printValueAsJSON` collapses any attrset carrying an `outPath` to
/// that string alone (`value-to-json.cc:100`, the derivation shorthand) and a
/// `sourceInfo` has one. And `outPath` is then overwritten with the store-path
/// escape, because JSON cannot carry string context and a flake source path
/// without its own `Opaque` element is a dependency that has quietly
/// vanished -- every derivation built from `self` would lose an input.
static nlohmann::json
flakeOverridesJSON(EvalState & state, const flake::LockedFlake & lockedFlake, const flake::LockFile::KeyMap & keyMap);

static LockedFlakeDescription describeLockedFlake(EvalState & state, const flake::LockedFlake & lockedFlake)
{
    /* Serialised once: the text is the lock argument and the key map names
       the nodes the overrides document covers. */
    auto [lockFileStr, keyMap] = lockedFlake.lockFile.to_string();
    return LockedFlakeDescription{
        .lockFile = std::move(lockFileStr),
        .overrides = flakeOverridesJSON(state, lockedFlake, keyMap),
    };
}

static nlohmann::json
flakeOverridesJSON(EvalState & state, const flake::LockedFlake & lockedFlake, const flake::LockFile::KeyMap & keyMap)
{

    auto overrides = nlohmann::json::object();
    for (auto & [node, sourcePath] : lockedFlake.nodePaths) {
        auto lockedNode = lockedFlake.lockFile.node(node);
        auto [storePath, subdir] = state.store->toStorePath(sourcePath.path.abs());

        Value vSourceInfo;
        emitTreeAttrs(
            state,
            storePath,
            lockedNode ? lockedNode->lockedRef.input : lockedFlake.flake.lockedRef.input,
            vSourceInfo,
            false,
            !lockedNode && lockedFlake.flake.forceDirty);

        state.forceAttrs(vSourceInfo, noPos, "while serialising a flake's source info");
        auto sourceInfo = nlohmann::json::object();
        for (auto & a : *vSourceInfo.attrs()) {
            NixStringContext context;
            sourceInfo[std::string(state.symbols[a.name])] =
                printValueAsJSON(state, true, *a.value, noPos, context, false);
        }
        sourceInfo["outPath"] = nlohmann::json::object({{"__storePath", state.store->printStorePath(storePath)}});

        auto key = keyMap.find(node);
        // cppnix asserts here. A throw rather than an assert because an
        // assert is compiled out of a release build, and a node with no key
        // would silently drop an override -- which is a flake input resolving
        // to the wrong tree, not a crash.
        if (key == keyMap.end())
            throw Error("rust-eval: a locked flake node has no key in the lock file");
        overrides[key->second] = nlohmann::json::object({
            {"sourceInfo", sourceInfo},
            {"dir", CanonPath(subdir).rel()},
        });
    }
    /* How much of the lock this document covers, which decides which half of
       `call-flake.nix` runs and is otherwise unobservable from outside.
       `computeLocks` fills `nodePaths` only for nodes it fetches, so a lock
       being created covers every node -- `hasOverride` is true everywhere and
       `fetchTreeFinal` is unreachable -- while an up-to-date lock keeps
       children lazily, leaves them out, and sends them through
       `fetchTreeFinal` instead.

       Emitted because a gate cannot otherwise tell a run that exercised the
       tree fetcher from one that did not, and the two look identical in every
       value they produce. `flake-inputs-parity.sh` reads this line and refuses
       a run in which no node reached the fetcher. */
    debug("rust-eval: flake overrides cover %d of %d lock node(s)", overrides.size(), keyMap.size());
    return overrides;
}

static RustEvaluand rustEvaluandOfLockedFlake(
    EvalState & state,
    const flake::LockedFlake & lockedFlake,
    Strings attrPaths,
    std::string flakeRef,
    std::optional<RustFlakeContext> flakeContext = std::nullopt)
{
    auto description = describeLockedFlake(state, lockedFlake);
    return RustEvaluand{
        .src = RustSource{.source = std::string(flake::callFlakeSource()), .baseDir = "/", .file = ""},
        .args =
            {
                RustArgument{.kind = RustArgument::Kind::Json, .text = nlohmann::json(description.lockFile).dump()},
                RustArgument{.kind = RustArgument::Kind::Json, .text = description.overrides.dump()},
                RustArgument{.kind = RustArgument::Kind::InternalPrimop, .text = "fetchFinalTree"},
            },
        .attrPaths = std::move(attrPaths),
        .indexLists = false,
        .flakeRef = std::move(flakeRef),
        .flakeContext = std::move(flakeContext),
    };
}

RustEvaluand rustEvaluandOfLockedFlake(EvalState & state, const flake::LockedFlake & lockedFlake)
{
    return rustEvaluandOfLockedFlake(state, lockedFlake, Strings{""}, lockedFlake.flake.lockedRef.to_string());
}

RustEvaluand rustEvaluandOf(
    SourceExprCommand & cmd, ref<EvalState> state, const std::optional<RustSource> & source, std::string_view prefix)
{
    // "." selects the whole value when no attribute path is supplied.
    auto attrPath = prefix == "." ? std::string() : std::string(prefix);

    if (source)
        return RustEvaluand{.src = *source, .args = {}, .attrPaths = {attrPath}, .autoArgs = rustAutoArgsOf(cmd)};

    // cppnix tries a store path first whenever the argument contains a slash
    // (`installables.cc`), and falls through to a flake reference when that
    // does not parse. Same order here, so the two backends disagree about
    // nothing: what changes is that a store path is refused rather than
    // served.
    if (prefix.find('/') != std::string_view::npos) {
        bool isStorePath = false;
        try {
            (void) InstallableDerivedPath::parse(state->store, prefix, ExtendedOutputsSpec::Default{});
            isStorePath = true;
        } catch (BadStorePath &) {
        } catch (Error &) {
        }
        if (isStorePath)
            refuse(
                refusalTokens::installable,
                "the store-path installable '%s' (this backend evaluates a flake, an '--expr' "
                "or a '--file'; a store path names something already built)",
                prefix);
    }

    auto [flakeRef, fragment] =
        parseFlakeRefWithFragment(fetchSettings, std::string{prefix}, absPath(cmd.getCommandBaseDir()));

    /* The installable cppnix would have built, used for the two rules that
       decide *what* is selected: the candidate attribute paths, and the lock.
       Constructing it evaluates nothing -- `getCursors` does, and is not
       called -- so this is the rules and not the C++ evaluator running. A
       second copy of `getActualAttrPaths`'s prefix ladder here is how the two
       backends would come to build different packages for the same command
       line. It also raises cppnix's own UsageError for `--arg` with a flake,
       which is why the arguments are carried on the other branch only. */
    InstallableFlake installable(
        &cmd,
        state,
        std::move(flakeRef),
        fragment,
        ExtendedOutputsSpec::Default{},
        cmd.getDefaultFlakeAttrPaths(),
        cmd.getDefaultFlakeAttrPathPrefixes(),
        cmd.lockFlags);

    return rustEvaluandOfInstallable(*state, installable);
}

Installables rustParseInstallables(SourceExprCommand & cmd, ref<Store> store, std::vector<std::string> ss)
{
    /* Every source-shape refusal and the `pure-eval` relaxation for `--file`,
       before the EvalState exists: its constructor captures the restricted
       accessor. Empty when the positional arguments are installables. */
    auto source = rustSourceOf(cmd);

    Installables result;
    for (auto & raw : ss) {
        auto [prefix, extendedOutputsSpec] = ExtendedOutputsSpec::parse(raw);

        /* A store path is evaluated by nobody. cppnix tries it first whenever
           the argument contains a slash and falls through to the flake reading
           on any failure (it keeps the exception and then overwrites it with
           the flake's, so the store-path error is never the one reported);
           same order, same parser, same fallthrough here, so the two backends
           cannot read one argument two ways. `Error`, not `...`: an
           `Interrupted` (a `BaseError`, deliberately not an `Error`) or a
           `bad_alloc` is not a failed parse and propagates. */
        if (!source && prefix.find('/') != std::string_view::npos) {
            try {
                result.push_back(
                    make_ref<InstallableDerivedPath>(
                        InstallableDerivedPath::parse(store, prefix, extendedOutputsSpec.raw)));
                continue;
            } catch (Error &) {
            }
        }

        /* Per positional argument, because each is its own flake reference
           when there is no `--file`, with its own lock. Nothing is asked here:
           the installable evaluates when its derivations are, so a command
           that refuses at `InstallableValue::require` refuses first. */
        // Opaque paths need no evaluator; legacy SSH stores cannot provide its filesystem accessor.
        auto state = cmd.getEvalState();
        result.push_back(
            make_ref<InstallableRustDerivation>(
                cmd, state, source, std::string(prefix), std::move(extendedOutputsSpec), raw));
    }
    return result;
}

DerivedPathsWithInfo InstallableRustDerivation::toDerivedPaths()
{
    if (!derivedPath) {
        /* One derivation comes back (a selection that is not a derivation
           refuses) with the outputs `meta.outputsToInstall` selected; an
           explicit `^out` or `^*` overrides that, as it does in
           `InstallableFlake::toDerivedPaths`. The Rust question reduces
           `meta.outputsToInstall` and checks `outputSpecified` either way:
           its answer is one memo row per derivation, independent of the
           spec, so a row an explicit query stored can never hand a default
           query the full output set. The price is that a malformed
           `meta.outputsToInstall` refuses under `^dev` where cppnix would
           not have read it: a refusal, never a different build. */
        auto evaluand = rustEvaluandOf(cmd, state, source, prefix);
        // Source installables auto-call the final function too, including
        // an empty attribute path. Rust applies the arguments and defaults;
        // the selection flag also keeps this operation in the memo key.
        evaluand.autoCall = source.has_value();
        auto drv = rustEvalDerivations(*state, evaluand);
        auto outputs = std::visit(
            overloaded{
                [&](const ExtendedOutputsSpec::Default &) -> OutputsSpec { return OutputsSpec::Names{drv.outputs}; },
                [&](const ExtendedOutputsSpec::Explicit & explicitOutputs) -> OutputsSpec { return explicitOutputs; },
            },
            extendedOutputsSpec.raw);
        derivedPath = DerivedPath::Built{
            .drvPath = makeConstantStorePathRef(drv.drvPath),
            .outputs = std::move(outputs),
        };
    }
    return {{
        .path = *derivedPath,
        .info = make_ref<ExtraPathInfo>(),
    }};
}

RustEvaluand rustEvaluandOfInstallable(EvalState & state, InstallableFlake & installable)
{
    /* The one thing this cannot serve, for the reason `rustFetchTree` gives
       and in the same words: `emitTreeAttrs` answers with a recording thunk
       per metadata attribute when the tracker is on, and the overrides
       document below forces every one of them. */
    if (state.readSetTracker)
        refuse(
            refusalTokens::unsupported,
            "a flake installable while the read-set tracker is on (the overrides this hands "
            "over are cppnix's emitTreeAttrs sets, which are per-attribute recording thunks "
            "under the tracker, and serialising them would both record reads nobody made and "
            "lose the ones the flake does make)");

    /* The lock graph stays C++ policy; each `flake.nix` it reads is a document
       from the selected backend (`flake::readFlake`, through the backend this
       file installs). The flake's outputs are evaluated by Rust from
       call-flake.nix and the lock document below. */
    auto lockedFlake = installable.getLockedFlake().get_ptr();
    // Resolution may introduce an explicit registry pin. The locked input is
    // deliberately excluded: fetching an unpinned workspace pins its HEAD.
    auto unpinned = [&](const fetchers::Input & input) {
        return !input.getRef() && !input.getRev() && !input.isLocked(state.fetchSettings);
    };
    Strings attrPaths;
    for (auto & candidate : installable.getActualAttrPaths())
        attrPaths.push_back(candidate);
    return rustEvaluandOfLockedFlake(
        state,
        *lockedFlake,
        std::move(attrPaths),
        installable.flakeRef.to_string(),
        RustFlakeContext{
            .nixpkgsFlakeRef = installable.nixpkgsFlakeRef(),
            .sourcePath = lockedFlake->flake.resolvedRef.input.getSourcePath(),
            .sourceStorePath =
                state.store->printStorePath(state.store->toStorePath(lockedFlake->flake.path.path.abs()).first),
            .canEditCheckout =
                unpinned(lockedFlake->flake.originalRef.input) && unpinned(lockedFlake->flake.resolvedRef.input),
        });
}

/// One field of a derivation-set answer: `<len>:<bytes>,`.
///
/// The evaluator's codec (`encode_derivation_set`): every field carries its
/// own length, so no byte of a name can spell a record boundary, and an
/// answer that ends mid-field is malformed rather than short.
static std::string_view takeNetstring(std::string_view & rest)
{
    auto colon = rest.find(':');
    if (colon == std::string_view::npos || colon == 0)
        throw Error("rust-eval: the evaluation cache holds a derivation-set answer with a malformed field length");
    size_t len = 0;
    for (char c : rest.substr(0, colon)) {
        if (c < '0' || c > '9')
            throw Error("rust-eval: the evaluation cache holds a derivation-set answer with a malformed field length");
        len = len * 10 + (c - '0');
    }
    rest.remove_prefix(colon + 1);
    if (rest.size() < len + 1 || rest[len] != ',')
        throw Error("rust-eval: the evaluation cache holds a derivation-set answer with a truncated field");
    auto field = rest.substr(0, len);
    rest.remove_prefix(len + 1);
    return field;
}

static std::vector<RustBuiltDerivation> decodeDerivationSet(EvalState & state, std::string_view encoded)
{
    std::vector<RustBuiltDerivation> found;
    while (!encoded.empty()) {
        auto drvPathText = takeNetstring(encoded);
        auto outputName = takeNetstring(encoded);
        // `PackageInfo::queryDrvPath`, then nix-build's own check: the path
        // must be a store path naming a derivation, and the output must be
        // named.
        auto drvPath = state.store->parseStorePath(drvPathText);
        drvPath.requireDerivation();
        if (outputName.empty())
            throw Error("derivation '%s' lacks an 'outputName' attribute", state.store->printStorePath(drvPath));
        found.push_back(RustBuiltDerivation{.drvPath = std::move(drvPath), .outputName = std::string(outputName)});
    }
    return found;
}

std::vector<RustBuiltDerivation> rustEvalDerivationSet(EvalState & state, const RustEvaluand & evaluand)
{
    RustQuestion<std::string> question(state, evaluand, IXE_QUESTION_DERIVATION_SET, 0);
    auto identity = [](const std::string & text) { return text; };
    if (auto answer = question.answered(identity)) {
        auto found = decodeDerivationSet(state, *answer);
        for (auto & built : found)
            rootServedDerivation(state, built.drvPath);
        return found;
    }
    auto & session = question.session;
    auto & root = question.root;

    // Every requested path contributes records; missing paths are errors.
    std::string encoded;
    for (size_t index = 0; index < evaluand.attrPaths.size(); ++index) {
        IxeHandle selected = 0;
        if (int rc = ixe_question_select_at(session.p, root.handle, index, &selected); rc != 0)
            session.fail(state, rc);
        IxeValue current;
        current.reset(session.p, selected);
        IxeString records;
        if (int rc = ixe_derivation_set(session.p, selected, &records.s); rc != 0)
            session.fail(state, rc);
        encoded += records.str();
    }

    auto filed = question.settle(std::move(encoded), identity, identity);
    return decodeDerivationSet(state, filed);
}

RustDerivation rustEvalDerivations(EvalState & state, const RustEvaluand & evaluand)
{
    /* One path, as in `rustEvalRender` and for the same reason. `nix build
       <flake>#attr` is the command warm starts were built for, and it is the
       one `mayBeMemoised` excluded. No rendering happens for this question;
       the field is in the key anyway only for the select shape. */
    RustQuestion<RustDerivation> question(state, evaluand, IXE_QUESTION_DERIVATION, IXE_RENDER_PLAIN);
    auto decode = [&](const std::string & encoded) { return decodeDerivation(state, encoded); };
    if (auto answer = question.answered(decode)) {
        rootServedDerivation(state, answer->drvPath);
        return std::move(*answer);
    }
    auto & session = question.session;
    auto & root = question.root;

    selectFrom(state, session, root);

    // cppnix's `getDerivations` accepts three shapes here: a derivation, a
    // path or string naming a store path, and an attribute set to recurse
    // into. Only the first is served. The other two are refused by name
    // rather than approximated, because both would change what gets built:
    // `trySinglePathToDerivedPaths` copies a path into the store, and the
    // recursion has `recurseForDerivations` rules of its own.
    if (ixe_value_type(session.p, root.handle) != IXE_TYPE_ATTRS)
        refuse(
            refusalTokens::notADerivation,
            "an installable that is not a derivation (this backend builds a derivation, and "
            "cppnix would also accept a store path or an attribute set to recurse into)");

    {
        IxeValue type;
        if (!selectOptional(state, session, root.handle, "type", type)
            || forceString(state, session, type, "the 'type' attribute", refusalTokens::notADerivation) != "derivation")
            refuse(
                refusalTokens::notADerivation,
                "an attribute set that is not a derivation (cppnix would recurse into it "
                "looking for derivations, which this backend does not do)");
    }

    StorePath drvPath = ({
        IxeValue attr;
        if (!selectOptional(state, session, root.handle, "drvPath", attr))
            throw Error("derivation does not contain a 'drvPath' attribute");
        auto text = forceString(state, session, attr, "the 'drvPath' attribute", refusalTokens::notADerivation);
        auto path = state.store->parseStorePath(text);
        // cppnix's `requireDerivation`, and its wording. A `drvPath` naming
        // something that is not a derivation would build the wrong thing.
        path.requireDerivation();
        std::move(path);
    });

    // The `outputs` list, then the reduction cppnix's
    // `queryOutputs(false, true)` applies to it.
    StringSet outputs;
    {
        IxeValue attr;
        if (selectOptional(state, session, root.handle, "outputs", attr)) {
            if (int rc = ixe_force(session.p, attr.handle); rc != 0)
                session.fail(state, rc);
            if (ixe_value_type(session.p, attr.handle) != IXE_TYPE_LIST)
                refuse(refusalTokens::notADerivation, "the 'outputs' attribute is not a list");
            size_t count = 0;
            if (int rc = ixe_list_len(session.p, attr.handle, &count); rc != 0)
                session.fail(state, rc);
            for (size_t i = 0; i < count; ++i) {
                IxeValue element;
                IxeHandle handle = 0;
                if (int rc = ixe_list_at(session.p, attr.handle, i, &handle); rc != 0)
                    session.fail(state, rc);
                element.reset(session.p, handle);
                outputs.insert(forceString(
                    state, session, element, "an element of the 'outputs' list", refusalTokens::notADerivation));
            }
        }
        // cppnix's default when there is no `outputs` attribute, and its
        // fallback when the list turned out empty.
        if (outputs.empty())
            outputs.insert("out");
    }

    // `outputSpecified` selects one output by name and is what `lib.getOutput`
    // sets. Refused rather than implemented: it reads `outputName`, which is
    // another attribute and another rule, and nothing this backend serves
    // today produces it.
    {
        IxeValue attr;
        if (selectOptional(state, session, root.handle, "outputSpecified", attr)) {
            if (int rc = ixe_force(session.p, attr.handle); rc != 0)
                session.fail(state, rc);
            if (ixe_value_type(session.p, attr.handle) != IXE_TYPE_BOOL)
                refuse(refusalTokens::outputsToInstall, "'outputSpecified' is not a boolean");
            /* The Boolean accessor, not a raw render: a Boolean rendered raw
               is cppnix's "cannot coerce a Boolean to a string", which turned
               this refusal into an error the cpp arm never raises. */
            int specified = 0;
            if (int rc = ixe_get_bool(session.p, attr.handle, &specified); rc != 0)
                session.fail(state, rc);
            if (specified)
                refuse(
                    refusalTokens::outputsToInstall, "'outputSpecified = true', which selects a single output by name");
        }
    }

    // `meta.outputsToInstall`, which nixpkgs sets on most packages: without
    // it a multi-output package would build every output where cppnix builds
    // the named ones, which is a different build rather than a missing
    // feature. Only the plain shape is reduced -- a list of strings, each of
    // them an output this derivation has. Anything else is refused, because
    // cppnix's `checkMeta` has rules for when a bad value falls back to the
    // full set, and mirroring those rules here would be a second
    // implementation of them for the two to disagree over.
    {
        IxeValue meta;
        if (selectOptional(state, session, root.handle, "meta", meta)) {
            if (int rc = ixe_force(session.p, meta.handle); rc != 0)
                session.fail(state, rc);
            if (ixe_value_type(session.p, meta.handle) != IXE_TYPE_ATTRS)
                refuse(refusalTokens::outputsToInstall, "'meta' is not an attribute set");
            IxeValue wanted;
            if (selectOptional(state, session, meta.handle, "outputsToInstall", wanted)) {
                if (int rc = ixe_force(session.p, wanted.handle); rc != 0)
                    session.fail(state, rc);
                if (ixe_value_type(session.p, wanted.handle) != IXE_TYPE_LIST)
                    refuse(refusalTokens::outputsToInstall, "'meta.outputsToInstall' is not a list");
                size_t count = 0;
                if (int rc = ixe_list_len(session.p, wanted.handle, &count); rc != 0)
                    session.fail(state, rc);
                StringSet reduced;
                for (size_t i = 0; i < count; ++i) {
                    IxeValue element;
                    IxeHandle handle = 0;
                    if (int rc = ixe_list_at(session.p, wanted.handle, i, &handle); rc != 0)
                        session.fail(state, rc);
                    element.reset(session.p, handle);
                    auto name = forceString(
                        state,
                        session,
                        element,
                        "an element of 'meta.outputsToInstall'",
                        refusalTokens::notADerivation);
                    if (!outputs.contains(name))
                        refuse(
                            refusalTokens::outputsToInstall,
                            "'meta.outputsToInstall' names '%s', which is not one of this "
                            "derivation's outputs",
                            name);
                    reduced.insert(std::move(name));
                }
                // An empty list would build nothing at all. cppnix reduces to
                // it happily; this refuses, because a build that silently
                // produces no output is the shape nobody notices.
                if (reduced.empty())
                    refuse(refusalTokens::outputsToInstall, "'meta.outputsToInstall' is empty");
                outputs = std::move(reduced);
            }
        }
    }

    return question.settle(
        RustDerivation{.drvPath = std::move(drvPath), .outputs = std::move(outputs)},
        [&](const RustDerivation & found) { return encodeDerivation(state, found); },
        decode);
}

StorePath rustEvalDerivationPath(EvalState & state, const RustEvaluand & evaluand)
{
    RustQuestion<StorePath> question(state, evaluand, IXE_QUESTION_DERIVATION_PATH, IXE_RENDER_PLAIN);
    auto decode = [&](const std::string & encoded) {
        auto path = state.store->parseStorePath(encoded);
        path.requireDerivation();
        return path;
    };
    if (auto answer = question.answered(decode))
        return *answer;
    auto & session = question.session;
    auto & root = question.root;

    selectFrom(state, session, root);
    if (ixe_value_type(session.p, root.handle) != IXE_TYPE_ATTRS)
        refuse(refusalTokens::notADerivation, "the value selected for nix develop is not an attribute set");
    IxeValue type;
    if (!selectOptional(state, session, root.handle, "type", type)
        || forceString(state, session, type, "the 'type' attribute", refusalTokens::notADerivation) != "derivation")
        refuse(refusalTokens::notADerivation, "the value selected for nix develop is not a derivation");
    IxeValue drvPathAttr;
    if (!selectOptional(state, session, root.handle, "drvPath", drvPathAttr))
        throw Error("derivation does not contain a 'drvPath' attribute");
    auto path = state.store->parseStorePath(
        forceString(state, session, drvPathAttr, "the 'drvPath' attribute", refusalTokens::notADerivation));
    path.requireDerivation();

    return question.settle(
        std::move(path), [&](const StorePath & found) { return state.store->printStorePath(found); }, decode);
}

RustSourcePosition rustEvalSourcePosition(EvalState & state, const RustEvaluand & evaluand)
{
    struct Answer
    {
        RustSourcePosition position;
        std::string encoded;
    };

    auto decode = [](const std::string & encoded) {
        IxeSourcePosition position{};
        IxeString error;
        if (ixe_source_position_decode(
                reinterpret_cast<const uint8_t *>(encoded.data()), encoded.size(), &position, &error.s)
            != 0)
            throw Error("%s", error.str());
        Finally release([&] { ixe_string_free(position.file); });
        return Answer{.position = {.file = position.file, .line = position.line}, .encoded = encoded};
    };
    RustQuestion<Answer> question(state, evaluand, IXE_QUESTION_SOURCE_POSITION, 0);
    if (auto answer = question.answered(decode))
        return std::move(answer->position);
    IxeString encoded;
    if (int rc = ixe_source_position_execute(question.session.p, question.root.handle, &encoded.s); rc != 0)
        question.session.fail(state, rc);
    question.session.drainWarnings();
    return question.settle(
                       decode(encoded.str()), [](const Answer & answer) { return answer.encoded; }, decode)
        .position;
}

RustSearchCatalogue rustEvalSearchCatalogue(EvalState & state, const RustEvaluand & evaluand, bool defaultFlakeRoots)
{
    auto decode = [](const std::string & encoded) { return RustSearchCatalogue(encoded); };
    RustQuestion<RustSearchCatalogue> question(
        state, evaluand, IXE_QUESTION_SEARCH_PACKAGES, defaultFlakeRoots ? 1 : 0);
    if (auto answer = question.answered(decode))
        return std::move(*answer);
    IxeString encoded;
    if (int rc = ixe_search_execute(question.session.p, question.root.handle, &encoded.s); rc != 0)
        question.session.fail(state, rc);
    question.session.drainWarnings();
    return question.settle(
        RustSearchCatalogue(encoded.str()), [](const RustSearchCatalogue & found) { return found.encode(); }, decode);
}

UnresolvedApp rustEvalApp(EvalState & state, const RustEvaluand & evaluand)
{
    RustQuestion<UnresolvedApp> question(state, evaluand, IXE_QUESTION_APP, IXE_RENDER_PLAIN);
    auto decode = [&](const std::string & encoded) { return decodeApp(*state.store, encoded); };
    if (auto answer = question.answered(decode))
        return materialiseApp(state, std::move(*answer));
    auto & session = question.session;
    auto & root = question.root;

    IxeHandle selectedHandle = 0;
    IxeString selectedPath, expectedType;
    if (int rc = ixe_question_select_app(session.p, root.handle, &selectedHandle, &selectedPath.s, &expectedType.s);
        rc != 0)
        session.fail(state, rc);
    root.reset(session.p, selectedHandle);
    auto selected = selectedPath.str();
    if (ixe_value_type(session.p, root.handle) != IXE_TYPE_ATTRS)
        throw Error("the value selected for 'nix run' is not an attribute set");
    IxeValue typeAttr;
    if (!selectOptional(state, session, root.handle, "type", typeAttr))
        throw Error("the value selected for 'nix run' has no 'type' attribute");
    auto type = forceString(state, session, typeAttr, "the 'type' attribute", refusalTokens::notAnApp);
    auto expected = expectedType.str();
    if (type != expected)
        throw Error("attribute '%s' should have type '%s'", selected, expected);

    /* Only the fields are gathered here; every rule that turns them into an
       app is `unresolvedAppOf`'s, shared with the C++ walk. */
    AppFields fields;
    if (type == "app") {
        IxeValue programAttr;
        if (!selectOptional(state, session, root.handle, "program", programAttr))
            throw Error("app definition does not contain a 'program' attribute");
        auto [program, context] =
            forceStringWithContext(state, session, programAttr, "the 'program' attribute", refusalTokens::notAnApp);
        fields.raw = AppFields::Program{.program = std::move(program), .context = std::move(context)};
    } else {
        auto required = [&](const char * name) {
            IxeValue attr;
            if (!selectOptional(state, session, root.handle, name, attr))
                throw Error("derivation does not contain a '%s' attribute", name);
            return forceString(state, session, attr, fmt("the '%s' attribute", name), refusalTokens::notAnApp);
        };
        auto drvPath = state.store->parseStorePath(required("drvPath"));
        drvPath.requireDerivation();
        auto outPath = required("outPath");
        auto outputName = required("outputName");
        auto name = required("name");
        std::optional<std::string> pname;
        IxeValue attr;
        if (selectOptional(state, session, root.handle, "pname", attr))
            pname = forceString(state, session, attr, "the 'pname' attribute", refusalTokens::notAnApp);
        std::optional<std::string> mainProgram;
        IxeValue meta;
        if (selectOptional(state, session, root.handle, "meta", meta)) {
            if (int rc = ixe_force(session.p, meta.handle); rc != 0)
                session.fail(state, rc);
            if (ixe_value_type(session.p, meta.handle) == IXE_TYPE_ATTRS
                && selectOptional(state, session, meta.handle, "mainProgram", attr))
                mainProgram =
                    forceString(state, session, attr, "the 'meta.mainProgram' attribute", refusalTokens::notAnApp);
        }
        fields.raw = AppFields::Derivation{
            .drvPath = std::move(drvPath),
            .outPath = std::move(outPath),
            .outputName = std::move(outputName),
            .name = std::move(name),
            .pname = std::move(pname),
            .mainProgram = std::move(mainProgram),
        };
    }
    auto found = unresolvedAppOf(fields);

    return materialiseApp(
        state,
        question.settle(
            std::move(found), [&](const UnresolvedApp & app) { return encodeApp(*state.store, app); }, decode));
}

RustFlakeCheckReport rustEvalFlakeCheck(
    EvalState & state, const RustEvaluand & evaluand, bool hydra, bool allSystems, bool keepGoing, bool evaluateOnly)
{
    const auto flags = (hydra ? 1 : 0) | (allSystems ? 2 : 0) | (keepGoing ? 4 : 0) | (evaluateOnly ? 8 : 0);
    RustQuestion<RustFlakeCheckReport> question(state, evaluand, IXE_QUESTION_FLAKE_CHECK, flags);
    auto decode = [&](const std::string & encoded) {
        auto view = [](std::string_view text) {
            return IxeBytes{reinterpret_cast<const unsigned char *>(text.data()), text.size()};
        };
        auto text = [](IxeBytes bytes) { return std::string(reinterpret_cast<const char *>(bytes.text), bytes.len); };
        IxeString error;
        IxeFlakeCheckReport * decoded = nullptr;
        if (ixe_flake_check_report_decode(
                view(encoded),
                view(state.store->storeDir),
                view(state.settings.getCurrentSystem()),
                flags,
                &decoded,
                &error.s))
            throw Error("invalid Rust flake-check report: %s", error.str());
        std::unique_ptr<IxeFlakeCheckReport, decltype(&ixe_flake_check_report_free)> owned(
            decoded, ixe_flake_check_report_free);
        auto checked = [](int status) {
            if (status)
                throw Error("invalid Rust flake-check report access");
        };
        IxeFlakeCheckCounts counts{};
        checked(ixe_flake_check_report_counts(owned.get(), &counts));
        RustFlakeCheckReport report;
        report.derivations.reserve(counts.derivations);
        for (size_t index = 0; index < counts.derivations; ++index) {
            IxeFlakeCheckDerivation item{};
            checked(ixe_flake_check_report_derivation(owned.get(), index, &item));
            report.derivations.push_back(
                RustFlakeCheckDerivation{
                    .attributePath = text(item.attribute_path),
                    .drvPath = state.store->parseStorePath(text(item.drv_path)),
                    .build = item.build != 0,
                });
        }
        report.errors.reserve(counts.errors);
        for (size_t index = 0; index < counts.errors; ++index) {
            IxeBytes message{};
            checked(ixe_flake_check_report_message(owned.get(), 0, index, &message));
            report.errors.push_back(text(message));
        }
        for (size_t index = 0; index < counts.omitted_systems; ++index) {
            IxeBytes system{};
            checked(ixe_flake_check_report_message(owned.get(), 1, index, &system));
            report.omittedSystems.insert(text(system));
        }
        return report;
    };
    auto decodeSuccess = [&](const std::string & encoded) {
        auto report = decode(encoded);
        if (!report.errors.empty())
            throw Error("a failed flake validation cannot be a cached success");
        return report;
    };
    if (auto answer = question.answered(decodeSuccess))
        return std::move(*answer);
    IxeString answer;
    if (auto status = ixe_flake_check_execute(question.session.p, question.root.handle, &answer.s)) {
        checkInterrupt();
        question.session.fail(state, status);
    }
    auto encoded = answer.str();
    auto report = decode(encoded);
    // Errors are reported to --keep-going, but never filed as memo successes.
    if (!report.errors.empty())
        return report;
    return question.settle(std::move(report), [&](const RustFlakeCheckReport &) { return encoded; }, decodeSuccess);
}

RustFlakeShowDocument rustEvalFlakeShow(
    EvalState & state,
    const RustEvaluand & evaluand,
    bool showLegacy,
    bool showAllSystems,
    bool withDescriptions,
    const std::string & localSystem)
{
    /* The three switches are in the memo key: a document walked without
       descriptions must not be served to a JSON run that prints them. */
    auto flags = (showLegacy ? 1 : 0) | (showAllSystems ? 2 : 0) | (withDescriptions ? 4 : 0);
    RustQuestion<RustFlakeShowDocument> question(state, evaluand, IXE_QUESTION_FLAKE_SHOW, flags);
    auto decode = [](const std::string & encoded) { return RustFlakeShowDocument::decode(encoded); };
    if (auto answer = question.answered(decode))
        return std::move(*answer);
    auto & session = question.session;
    auto & root = question.root;
    selectFrom(state, session, root);

    auto names = [&](IxeHandle handle) {
        IxeNames buffer;
        if (int rc = ixe_attrs_names(session.p, handle, &buffer.p, &buffer.len); rc != 0)
            session.fail(state, rc);
        return buffer.set();
    };
    auto child = [&](IxeHandle handle, const std::string & name) {
        IxeValue value;
        if (!selectOptional(state, session, handle, name, value))
            throw Error("attribute '%s' disappeared while walking flake outputs", name);
        return value;
    };
    auto typeName = ixeTypeName;
    auto optionalString = [&](IxeHandle handle,
                              const std::string & name,
                              const std::vector<std::string> & path) -> std::optional<std::string> {
        if (int rc = ixe_force(session.p, handle); rc != 0)
            session.fail(state, rc);
        if (ixe_value_type(session.p, handle) != IXE_TYPE_ATTRS)
            return std::nullopt;
        IxeValue value;
        if (!selectOptional(state, session, handle, name, value))
            return std::nullopt;
        if (int rc = ixe_force(session.p, value.handle); rc != 0)
            session.fail(state, rc);
        auto type = ixe_value_type(session.p, value.handle);
        if (type != IXE_TYPE_STRING && type != IXE_TYPE_PATH) {
            auto fieldPath = path;
            fieldPath.push_back(name);
            state.error<TypeError>("'%s' is not a string but %s", showAttrPath(fieldPath), typeName(type)).debugThrow();
        }
        return forceString(
            state, session, value, "flake output attribute '" + name + "'", refusalTokens::notADerivation);
    };
    auto isDerivation = [&](IxeHandle handle, const std::vector<std::string> & path) {
        if (int rc = ixe_force(session.p, handle); rc != 0)
            session.fail(state, rc);
        if (ixe_value_type(session.p, handle) != IXE_TYPE_ATTRS)
            return false;
        auto type = optionalString(handle, "type", path);
        return type && *type == "derivation";
    };
    auto requiredString = [&](IxeHandle handle, const std::string & name, const std::vector<std::string> & path) {
        auto value = optionalString(handle, name, path);
        if (!value) {
            auto fieldPath = path;
            fieldPath.push_back(name);
            throw Error("attribute '%s' does not exist", showAttrPath(fieldPath));
        }
        return *value;
    };
    auto description = [&](IxeHandle handle, const std::vector<std::string> & path) -> std::optional<std::string> {
        IxeValue meta;
        if (!selectOptional(state, session, handle, "meta", meta))
            return std::nullopt;
        if (int rc = ixe_force(session.p, meta.handle); rc != 0)
            session.fail(state, rc);
        if (ixe_value_type(session.p, meta.handle) != IXE_TYPE_ATTRS)
            return std::nullopt;
        auto metaPath = path;
        metaPath.push_back("meta");
        return optionalString(meta.handle, "description", metaPath);
    };

    std::function<bool(IxeHandle, const std::vector<std::string> &, const std::string &)> hasContent;
    hasContent = [&](IxeHandle handle, const std::vector<std::string> & path, const std::string & name) {
        auto nextPath = path;
        nextPath.push_back(name);
        auto value = child(handle, name);
        try {
            if (((nextPath[0] == "apps" || nextPath[0] == "checks" || nextPath[0] == "devShells"
                  || nextPath[0] == "legacyPackages" || nextPath[0] == "packages")
                 && (nextPath.size() == 1 || nextPath.size() == 2))
                || (nextPath.size() == 1
                    && (nextPath[0] == "formatter" || nextPath[0] == "nixosConfigurations"
                        || nextPath[0] == "nixosModules" || nextPath[0] == "overlays"))) {
                for (auto & nested : names(value.handle))
                    if (hasContent(value.handle, nextPath, nested))
                        return true;
                return false;
            }
            return true;
        } catch (EvalError &) {
            return true;
        }
    };

    RustFlakeShowDocument document;
    std::function<uint64_t(IxeHandle, const std::vector<std::string> &)> visit;
    visit = [&](IxeHandle handle, const std::vector<std::string> & path) -> uint64_t {
        unsigned int nodeKind = IXE_FLAKE_SHOW_BRANCH;
        std::string nodeName;
        std::optional<std::string> nodeDescription;
        std::map<std::string, uint64_t> nodeChildren;
        auto recurse = [&]() {
            for (auto & name : names(handle)) {
                if (!hasContent(handle, path, name))
                    continue;
                auto value = child(handle, name);
                auto nextPath = path;
                nextPath.push_back(name);
                nodeChildren.emplace(name, visit(value.handle, nextPath));
            }
        };
        auto derivation = [&]() {
            nodeKind = IXE_FLAKE_SHOW_DERIVATION;
            nodeName = requiredString(handle, "name", path);
            /* `meta.description` is read only when the output will print it
               (JSON), as cppnix does: forcing it for a text listing would
               turn a failing description thunk into a failed command. */
            if (withDescriptions)
                nodeDescription = description(handle, path).value_or("");
        };
        auto omitIFD = [&]() {
            nodeKind = IXE_FLAKE_SHOW_OMITTED_IFD;
            nodeName.clear();
            nodeDescription.reset();
            nodeChildren.clear();
        };

        try {
            if (path.empty()
                || (path.size() == 1
                    && (path[0] == "defaultPackage" || path[0] == "devShell" || path[0] == "formatter"
                        || path[0] == "nixosConfigurations" || path[0] == "nixosModules" || path[0] == "defaultApp"
                        || path[0] == "templates" || path[0] == "overlays"))
                || ((path.size() == 1 || path.size() == 2)
                    && (path[0] == "checks" || path[0] == "packages" || path[0] == "devShells" || path[0] == "apps"))) {
                recurse();
            } else if (
                (path.size() == 2 && (path[0] == "defaultPackage" || path[0] == "devShell" || path[0] == "formatter"))
                || (path.size() == 3 && (path[0] == "checks" || path[0] == "packages" || path[0] == "devShells"))) {
                if (!showAllSystems && path[1] != localSystem)
                    nodeKind = IXE_FLAKE_SHOW_OMITTED_SYSTEM;
                else
                    try {
                        if (isDerivation(handle, path))
                            derivation();
                        else
                            nodeKind = IXE_FLAKE_SHOW_NON_DERIVATION;
                    } catch (IFDError &) {
                        omitIFD();
                    }
            } else if (!path.empty() && path[0] == "hydraJobs") {
                try {
                    if (isDerivation(handle, path))
                        derivation();
                    else
                        recurse();
                } catch (IFDError &) {
                    omitIFD();
                }
            } else if (!path.empty() && path[0] == "legacyPackages") {
                if (path.size() == 1)
                    recurse();
                else if (!showLegacy)
                    nodeKind = IXE_FLAKE_SHOW_OMITTED_LEGACY;
                else if (!showAllSystems && path[1] != localSystem)
                    nodeKind = IXE_FLAKE_SHOW_OMITTED_SYSTEM;
                else
                    try {
                        if (isDerivation(handle, path))
                            derivation();
                        else if (path.size() <= 2)
                            recurse();
                        else
                            nodeKind = IXE_FLAKE_SHOW_EMPTY;
                    } catch (IFDError &) {
                        omitIFD();
                    }
            } else if ((path.size() == 2 && path[0] == "defaultApp") || (path.size() == 3 && path[0] == "apps")) {
                auto type = optionalString(handle, "type", path);
                if (!type || *type != "app")
                    state.error<EvalError>("not an app definition").debugThrow();
                nodeKind = IXE_FLAKE_SHOW_APP;
                if (auto text = description(handle, path))
                    nodeDescription = *text;
            } else if (
                (path.size() == 1 && path[0] == "defaultTemplate") || (path.size() == 2 && path[0] == "templates")) {
                nodeKind = IXE_FLAKE_SHOW_TEMPLATE;
                nodeDescription = requiredString(handle, "description", path);
            } else {
                nodeKind =
                    (path.size() == 1 && path[0] == "overlay") || (path.size() == 2 && path[0] == "overlays")
                        ? IXE_FLAKE_SHOW_NIXPKGS_OVERLAY
                    : path.size() == 2 && path[0] == "nixosConfigurations" ? IXE_FLAKE_SHOW_NIXOS_CONFIGURATION
                    : (path.size() == 1 && path[0] == "nixosModule") || (path.size() == 2 && path[0] == "nixosModules")
                        ? IXE_FLAKE_SHOW_NIXOS_MODULE
                        : IXE_FLAKE_SHOW_UNKNOWN;
            }
        } catch (EvalError &) {
            if (path.empty() || path[0] != "legacyPackages")
                throw;
            nodeKind = IXE_FLAKE_SHOW_EMPTY;
            nodeName.clear();
            nodeDescription.reset();
            nodeChildren.clear();
        }
        return document.addNode(nodeKind, nodeName, nodeDescription, nodeChildren);
    };

    /* cppnix's flake show walks the `outputs` attribute of the called flake
       (`flake.cc`, `vFlake.attrs()->get("outputs")`), never the merged call
       result, whose metadata -- `_type`, `inputs`, `sourceInfo`, the
       timestamps -- sits beside the outputs at the top level and would walk
       as so many unknown-typed outputs. */
    auto outputs = child(root.handle, "outputs");
    document.finish(visit(outputs.handle, {}));
    return question.settle(
        std::move(document), [](const RustFlakeShowDocument & document) { return document.encode(); }, decode);
}

std::string rustLanguageDocs()
{
    IxeString docs;
    IxeString error;
    if (ixe_language_docs(&docs.s, &error.s) != 0)
        throw Error("Rust language catalogue: %s", error.str());
    return docs.str();
}

void rustSetHostBuildIdentity(std::string_view identity)
{
    setOnce(
        "the host build identity",
        ixe_set_host_build_identity(reinterpret_cast<const unsigned char *>(identity.data()), identity.size()));
}

} // namespace nix
