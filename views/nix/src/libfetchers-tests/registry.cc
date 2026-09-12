#include "nix/fetchers/fetch-settings.hh"
#include "nix/fetchers/registry.hh"
#include "nix/util/file-system.hh"
#include "nix/util/finally.hh"

#include <gtest/gtest.h>

#include <utility>

namespace nix::fetchers {

TEST(Registry, customPathAndReplacementAreObserved)
{
    auto & features = experimentalFeatureSettings.experimentalFeatures.get();
    auto savedFeatures = features;
    Finally restoreFeatures([&]() { features = std::move(savedFeatures); });
    features.insert(Xp::Flakes);

    auto directory = createTempDir();
    AutoDelete cleanup(directory, true);
    Settings settings;
    auto first = directory / "first.json";
    auto second = directory / "second.json";
    const std::string empty = R"({"version":2,"flakes":[]})";
    const std::string populated = R"({"version":2,"flakes":[{
        "from":{"type":"indirect","id":"foo"},
        "to":{"type":"path","path":"/nix/store/source"}
    }]})";
    writeFile(first, empty);
    writeFile(second, populated);

    EXPECT_TRUE(getCustomRegistry(settings, first)->entries().empty());
    auto selected = getCustomRegistry(settings, second);
    auto entries = selected->entries();
    ASSERT_EQ(entries.size(), 1);
    EXPECT_EQ(getStrAttr(entries.front().from.attrs, "id"), "foo");

    writeFile(first, populated);
    EXPECT_EQ(getCustomRegistry(settings, first)->entries().size(), 1);
    writeFile(second, empty);
    EXPECT_TRUE(getCustomRegistry(settings, second)->entries().empty());
}

} // namespace nix::fetchers
