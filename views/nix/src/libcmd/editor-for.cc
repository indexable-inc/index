#include "nix/cmd/editor-for.hh"
#include "nix/util/environment-variables.hh"
#include "nix/util/source-path.hh"
#include "nix/expr/eval.hh"
#include "nix/store/store-api.hh"
#include "nix/store/local-fs-store.hh"

namespace nix {

Strings editorFor(EvalState & state, const SourcePath & file, uint32_t line)
{
    auto path = file.getPhysicalPath();
    /* A mounted input serves its files out of an object store (jj's, or
       git's) and has no physical path; its store path is a lazy mount that
       exists on disk only once something forces the copy. Opening a file
       for editing is such a force: the same road a build takes for a source
       it reads (`ensureLazyPathCopied`), landing on the same store path. */
    if (!path && state.store->isInStore(file.path.abs())) {
        auto [storePath, rest] = state.store->toStorePath(file.path.abs());
        /* The directory on disk. Under a chroot store (`--store /x`) the
           real store directory differs from the logical one, and only a
           local filesystem store has one at all: anything else has nowhere
           to force the file onto, and guessing the logical path would hand
           the editor a path that may not exist. */
        auto * fsStore = dynamic_cast<LocalFSStore *>(&*state.store);
        if (!fsStore)
            throw Error(
                "cannot open '%s' in an editor: it is served from a mounted input, and store '%s' exposes no "
                "local filesystem paths to force it onto",
                file,
                state.store->config.getHumanReadableURI());
        state.ensureLazyPathCopied(storePath);
        path = std::filesystem::path(
            fsStore->toRealPath(storePath).string() + (rest.isRoot() ? "" : std::string(rest.abs())));
    }
    if (!path)
        throw Error("cannot open '%s' in an editor because it has no physical path", file);
    auto editor = getEnv("EDITOR").value_or("cat");
    auto args = tokenizeString<Strings>(editor);
    if (line > 0
        && (editor.find("emacs") != std::string::npos || editor.find("nano") != std::string::npos
            || editor.find("vim") != std::string::npos || editor.find("kak") != std::string::npos))
        args.push_back(fmt("+%d", line));
    args.push_back(path->string());
    return args;
}

} // namespace nix
