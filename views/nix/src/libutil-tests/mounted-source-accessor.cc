#include "nix/util/memory-source-accessor.hh"
#include "nix/util/mounted-source-accessor.hh"
#include "nix/util/fs-sink.hh"

#include <gtest/gtest.h>

namespace nix {

TEST(MountedSourceAccessor, reverseMapsAUniqueChildAccessorForSiblingReads)
{
    auto root = make_ref<MemorySourceAccessor>();
    auto child = make_ref<MemorySourceAccessor>();
    MemorySink sink{*child};
    sink.createDirectory(CanonPath::root);
    sink.createRegularFile(CanonPath("/dep.nix"), [](CreateRegularFileSink & file) {
        file("dependency");
    });
    auto mounted = makeMountedSourceAccessor({
        {CanonPath::root, root},
        {CanonPath("/nix/store/00000000000000000000000000000000-source"), child},
    });

    auto found = mounted->findMount(*child);
    EXPECT_EQ(found, CanonPath("/nix/store/00000000000000000000000000000000-source"));
    ASSERT_TRUE(found);
    auto recovered = mounted->getMount(*found);
    ASSERT_TRUE(recovered);
    EXPECT_EQ(recovered->readFile(CanonPath("/dep.nix")), "dependency");
    EXPECT_EQ(mounted->findMount(*root), CanonPath::root);
}

TEST(MountedSourceAccessor, doesNotInventANameForAnAmbiguousChildAccessor)
{
    auto root = make_ref<MemorySourceAccessor>();
    auto child = make_ref<MemorySourceAccessor>();
    auto mounted = makeMountedSourceAccessor({
        {CanonPath::root, root},
        {CanonPath("/nix/store/00000000000000000000000000000000-one"), child},
        {CanonPath("/nix/store/11111111111111111111111111111111-two"), child},
    });

    EXPECT_EQ(mounted->findMount(*child), std::nullopt);
}

} // namespace nix
