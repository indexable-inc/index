#include "nix/fetchers/fetch-settings.hh"
#include "nix/fetchers/attrs.hh"
#include "nix/fetchers/fetchers.hh"
#include "nix/util/experimental-features.hh"
#include "nix/util/hash.hh"
#include "nix/util/finally.hh"
#include "nix/util/tests/gmock-matchers.hh"

#include <gtest/gtest.h>
#include <gmock/gmock.h>

#include <set>
#include <string>
#include <utility>

namespace nix {

using fetchers::Attr;
using fetchers::parseRev;

/* A commit id from jj's Git backend, which is an ordinary Git commit hash. */
constexpr std::string_view sha1Rev = "0123456789abcdef0123456789abcdef01234567";

/* A commit id from jj's native backend: 32 BLAKE3 bytes, so 64 hexadecimal
   characters. Nothing accepted this string before `parseRev` existed --
   `Hash::parseAny(s, HashAlgorithm::SHA1)` rejects it on length, and that is
   the parse both `jj.cc` and `Input::getRev` used -- which is what made a
   native-backend repository unfetchable rather than merely slow. */
constexpr std::string_view blake3Rev = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";

TEST(parseRev, sha1LengthParsesAsSha1)
{
    auto rev = parseRev(sha1Rev);
    EXPECT_EQ(rev.algo, HashAlgorithm::SHA1);
    EXPECT_EQ(rev.gitRev(), sha1Rev);
}

TEST(parseRev, blake3LengthParsesAsBlake3)
{
    auto rev = parseRev(blake3Rev);
    EXPECT_EQ(rev.algo, HashAlgorithm::BLAKE3);
    EXPECT_EQ(rev.gitRev(), blake3Rev);
}

/* The `blake3-hashes` experimental feature governs BLAKE3 as a *store* content
   address, which a user opts into, and not the hash a repository happens to
   name its commits with. A revision therefore parses with the feature off. */
TEST(parseRev, blake3RevNeedsNoExperimentalFeature)
{
    auto & features = experimentalFeatureSettings.experimentalFeatures.get();
    auto savedFeatures = features;
    Finally restoreFeatures([&]() { features = std::move(savedFeatures); });
    features.erase(Xp::BLAKE3Hashes);

    /* Control, decorrelated from the assertion below: with the feature off,
       BLAKE3 as a store hash is still refused. Without this the test would
       also pass if the gate had simply been lifted everywhere, which is a
       different (and much larger) change than the one being asserted. */
    EXPECT_THROW(hashString(HashAlgorithm::BLAKE3, "abc"), MissingExperimentalFeature);

    EXPECT_EQ(parseRev(blake3Rev).algo, HashAlgorithm::BLAKE3);
}

/* `parseRev` took over a parse that accepted more renderings than base-16, so
   narrowing it would quietly reject `rev` attributes already written that way. */
TEST(parseRev, sha1RevInNix32StillParses)
{
    auto nix32 = Hash::parseAny(sha1Rev, HashAlgorithm::SHA1).to_string(HashFormat::Nix32, false);
    ASSERT_NE(nix32, sha1Rev);

    auto rev = parseRev(nix32);
    EXPECT_EQ(rev.algo, HashAlgorithm::SHA1);
    EXPECT_EQ(rev.gitRev(), sha1Rev);
}

/* Lengths that are neither, including one either side of each accepted length.
   None of them collides with the base-32 (32 characters) or base-64 (28)
   rendering of a SHA-1, which `parseRev` still accepts. */
TEST(parseRev, otherLengthsAreRejected)
{
    for (auto length : {size_t(0), size_t(7), size_t(39), size_t(41), size_t(63), size_t(65)}) {
        auto bad = std::string(length, 'a');
        EXPECT_THROW(parseRev(bad), BadHash) << "accepted a " << length << "-character revision";
    }
}

TEST(parseRev, blake3LengthMustStillBeHex)
{
    EXPECT_THROW(parseRev(std::string(64, 'z')), BadHash);
}

class JjInputTest : public ::testing::Test
{
    std::set<ExperimentalFeature> savedFeatures = experimentalFeatureSettings.experimentalFeatures.get();

public:
    ~JjInputTest() override
    {
        experimentalFeatureSettings.experimentalFeatures.get() = std::move(savedFeatures);
    }

    void SetUp() override
    {
        experimentalFeatureSettings.experimentalFeatures.get().insert(Xp::Flakes);
    }
};

/* The round trip that broke the fetcher even once the commit ids parsed:
   `jj.cc` stores `rev` as bare hexadecimal (`Hash::gitRev()`), and everything
   done with the input afterwards -- `toURL`, `isLocked`, `getFingerprint`,
   `getAccessorFromRev` -- reads it back through `Input::getRev()`. A
   64-character rev threw there, so a native-backend fetch died immediately
   after succeeding. */
TEST_F(JjInputTest, bareBlake3RevRoundTrips)
{
    fetchers::Settings fetchSettings;

    auto attrs = fetchers::Attrs{
        {"type", Attr("jj")},
        {"url", Attr("file:///no/such/repo")},
        {"rev", Attr(std::string(blake3Rev))},
    };

    auto input = fetchers::Input::fromAttrs(fetchSettings, fetchers::Attrs(attrs));

    auto rev = input.getRev();
    ASSERT_TRUE(rev);
    EXPECT_EQ(rev->algo, HashAlgorithm::BLAKE3);
    EXPECT_EQ(rev->gitRev(), blake3Rev);

    /* `toURL` re-renders the rev; a lockfile write and `nix flake metadata`
       both go through it. */
    EXPECT_NE(input.toURLString().find("rev=" + std::string(blake3Rev)), std::string::npos)
        << "rev missing from " << input.toURLString();

    auto input2 = fetchers::Input::fromAttrs(fetchSettings, input.toAttrs());
    EXPECT_EQ(input, input2);
    EXPECT_EQ(input.toAttrs(), input2.toAttrs());
}

TEST_F(JjInputTest, nativeInputRejectsSha1Rev)
{
    fetchers::Settings fetchSettings;
    EXPECT_THAT(
        [&]() {
            fetchers::Input::fromAttrs(
                fetchSettings,
                fetchers::Attrs{
                    {"type", Attr("jj")},
                    {"url", Attr("file:///no/such/repo")},
                    {"rev", Attr(std::string(sha1Rev))},
                });
        },
        ::testing::ThrowsMessage<Error>(testing::HasSubstrIgnoreANSIMatcher("not a Jujutsu native commit id")));
}

TEST_F(JjInputTest, gitFileInputAcceptsSha1Rev)
{
    fetchers::Settings fetchSettings;
    auto input = fetchers::Input::fromURL(fetchSettings, "git+file:///no/such/repo?rev=" + std::string(sha1Rev));

    EXPECT_EQ(fetchers::getStrAttr(input.toAttrs(), "type"), "git");
    auto rev = input.getRev();
    ASSERT_TRUE(rev);
    EXPECT_EQ(rev->algo, HashAlgorithm::SHA1);
    EXPECT_EQ(rev->gitRev(), sha1Rev);
}

} // namespace nix
