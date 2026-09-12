#include <gtest/gtest.h>

#include <thread>
#include <vector>

#include <boost/unordered/unordered_flat_set.hpp>

#include "nix/fetchers/filtering-source-accessor.hh"
#include "nix/util/memory-source-accessor.hh"

namespace nix {

/**
 * `AllowListSourceAccessor::allowPrefix` used to insert into an unsynchronised
 * `std::set`. Under the parallel evaluator two threads reaching
 * `EvalState::allowPath` at once corrupted the red-black tree and dereferenced
 * null inside `std::__tree_balance_after_insert`, which showed up as a SIGSEGV
 * in one of nine twelve-host evaluations. Hammering the insert path directly
 * makes that deterministic instead of a one-in-nine flake.
 */
TEST(AllowListSourceAccessor, concurrentAllowPrefixDoesNotCorruptTheSet)
{
    constexpr size_t nThreads = 16;
    constexpr size_t nPerThread = 4000;

    auto accessor =
        AllowListSourceAccessor::create(make_ref<MemorySourceAccessor>(), {}, {}, [](const CanonPath & path) {
            return RestrictedPathError("access to '%s' is forbidden", path);
        });

    std::vector<std::thread> threads;
    for (size_t t = 0; t < nThreads; ++t)
        threads.emplace_back([&accessor, t]() {
            for (size_t i = 0; i < nPerThread; ++i) {
                accessor->allowPrefix(CanonPath("/t" + std::to_string(t) + "/p" + std::to_string(i)));
                /* Readers share the tree with the writers, so interleave them:
                   a reader walking a node another thread is rebalancing is the
                   other half of the original crash. */
                (void) accessor->isAllowed(CanonPath("/t0/p0/somewhere/deeper"));
            }
        });
    for (auto & t : threads)
        t.join();

    /* A crash is the loud failure. This is the quiet one: an unsynchronised
       insert can also simply lose a prefix, which would silently grant or deny
       the wrong path rather than crash. */
    for (size_t t = 0; t < nThreads; ++t)
        for (size_t i = 0; i < nPerThread; ++i)
            ASSERT_TRUE(accessor->isAllowed(CanonPath("/t" + std::to_string(t) + "/p" + std::to_string(i))))
                << "prefix /t" << t << "/p" << i << " was lost";
}

/* A tree below the filter that names its subtrees, standing in for an
   accessor over a Merkle object store. */
struct SubtreeNamingAccessor : MemorySourceAccessor
{
    std::shared_ptr<SourceAccessor> getSubtree(const CanonPath & path) override
    {
        return make_ref<MemorySourceAccessor>().get_ptr();
    }
};

static ref<SubtreeNamingAccessor> namingTree()
{
    auto next = make_ref<SubtreeNamingAccessor>();
    next->addFile(CanonPath("/a/b/c/f"), "x");
    next->addFile(CanonPath("/a/d/f"), "x");
    next->addFile(CanonPath("/x/f"), "x");
    next->addFile(CanonPath("/z/f"), "x");
    return next;
}

static MakeNotAllowedError forbidden()
{
    return [](const CanonPath & path) { return RestrictedPathError("access to '%s' is forbidden", path); };
}

/* A subtree accessor serves every child unfiltered, so the filter may hand
   one out only where its admission is prefix-closed: under an allowed
   prefix. The other admissions (`isAllowed` is true of `/a` when `/a/b/c` is
   listed, so directories on the way down can be listed; an exact path in
   `allowedPaths`) do not extend to children, and get no subtree. */
TEST(AllowListSourceAccessor, subtreeOnlyUnderAnAllowedPrefix)
{
    auto accessor = AllowListSourceAccessor::create(
        namingTree(),
        std::set<CanonPath>{CanonPath("/a/b")},
        boost::unordered_flat_set<CanonPath>{CanonPath("/x")},
        forbidden());

    /* Under the prefix, and the prefix itself. */
    EXPECT_NE(accessor->getSubtree(CanonPath("/a/b/c")), nullptr);
    EXPECT_NE(accessor->getSubtree(CanonPath("/a/b")), nullptr);

    /* Allowed only as an ancestor of the prefix: `/a/d` beneath it is not. */
    EXPECT_TRUE(accessor->isAllowed(CanonPath("/a")));
    EXPECT_FALSE(accessor->isAllowed(CanonPath("/a/d")));
    EXPECT_EQ(accessor->getSubtree(CanonPath("/a")), nullptr);

    /* Allowed as an exact path: `/x/f` beneath it is not. */
    EXPECT_TRUE(accessor->isAllowed(CanonPath("/x")));
    EXPECT_FALSE(accessor->isAllowed(CanonPath("/x/f")));
    EXPECT_EQ(accessor->getSubtree(CanonPath("/x")), nullptr);

    /* Not allowed at all: refused here, by name, not on a later read. */
    EXPECT_THROW(accessor->getSubtree(CanonPath("/z")), RestrictedPathError);

    /* Control: the tree below does name `/a` and `/x`, so the nullptrs above
       are the filter's, not its. */
    EXPECT_NE(namingTree()->getSubtree(CanonPath("/a")), nullptr);
    EXPECT_NE(namingTree()->getSubtree(CanonPath("/x")), nullptr);
}

/* A filter with no prefix-closed admission never names a subtree, however
   permissive its predicate: the predicate is per path and a subtree would
   bypass it for every child. */
struct AllowEverything : FilteringSourceAccessor
{
    using FilteringSourceAccessor::FilteringSourceAccessor;

    bool isAllowed(const CanonPath & path) override
    {
        return true;
    }
};

TEST(FilteringSourceAccessor, genericFilterNamesNoSubtree)
{
    auto next = namingTree();
    AllowEverything accessor(SourcePath(next), forbidden());
    EXPECT_EQ(accessor.getSubtree(CanonPath("/a")), nullptr);
    EXPECT_NE(next->getSubtree(CanonPath("/a")), nullptr);
}

} // namespace nix
