#include <gtest/gtest.h>

#include "nix/util/memory-source-accessor.hh"
#include "nix/util/source-accessor.hh"

namespace nix {

/* An in-memory tree that can name its subtrees, standing in for an accessor
   over a Merkle object store: any directory it has is a subtree object. */
struct SubtreeNamingAccessor : MemorySourceAccessor
{
    std::shared_ptr<SourceAccessor> getSubtree(const CanonPath & path) override
    {
        auto st = lstat(path);
        if (st.type != tDirectory)
            throw NotADirectory("path '%s' is not a directory", showPath(path));
        return make_ref<MemorySourceAccessor>().get_ptr();
    }
};

static ref<SubtreeNamingAccessor> naming(std::initializer_list<std::string> files)
{
    auto accessor = make_ref<SubtreeNamingAccessor>();
    for (auto & file : files)
        accessor->addFile(CanonPath(file), "x");
    return accessor;
}

static ref<MemorySourceAccessor> plain(std::initializer_list<std::string> files)
{
    auto accessor = make_ref<MemorySourceAccessor>();
    for (auto & file : files)
        accessor->addFile(CanonPath(file), "x");
    return accessor;
}

/* The evaluator's impure root filesystem is a union of the real filesystem
   over the store mounts, and a lazily mounted store path that later got
   materialized is present in both layers. A subtree named by the union must
   not depend on which layers happen to have the path, or a relative flake
   input's store path would change when its parent is copied to the store. */
TEST(UnionSourceAccessor, subtreeNamedOnlyWhenOneLayerHasThePath)
{
    auto d = CanonPath("/d");

    /* One layer, and it names subtrees: its answer stands. */
    EXPECT_NE(makeUnionSourceAccessor({naming({"/d/f"}), plain({"/e/f"})})->getSubtree(d), nullptr);

    /* One layer, and it cannot name subtrees: nothing beneath it to hide. */
    EXPECT_EQ(makeUnionSourceAccessor({naming({"/e/f"}), plain({"/d/f"})})->getSubtree(d), nullptr);

    /* Both layers have the path: the union's view of `/d` is a merge, not an
       object, whichever layer is on top. */
    EXPECT_EQ(makeUnionSourceAccessor({naming({"/d/f"}), plain({"/d/g"})})->getSubtree(d), nullptr);
    EXPECT_EQ(makeUnionSourceAccessor({plain({"/d/g"}), naming({"/d/f"})})->getSubtree(d), nullptr);

    /* Control for the merge rule: the same top layer alone does answer. */
    EXPECT_NE(makeUnionSourceAccessor({naming({"/d/f"})})->getSubtree(d), nullptr);

    /* No layer has the path: an error naming it, like every other read. */
    EXPECT_THROW(makeUnionSourceAccessor({naming({"/e/f"}), plain({"/e/g"})})->getSubtree(d), FileNotFound);
}

} // namespace nix
