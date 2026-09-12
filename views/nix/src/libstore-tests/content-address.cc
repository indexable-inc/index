#include <gtest/gtest.h>
#include <gmock/gmock.h>

#include "nix/store/content-address.hh"
#include "nix/util/memory-source-accessor.hh"
#include "nix/util/archive.hh"
#include "nix/util/tests/json-characterization.hh"

namespace nix {

TEST(ContentAddressHash, SelfReferenceRewritePreservesAddressButZeroingDoesNot)
{
    const std::string before(32, 'a');
    const std::string after(32, 'b');
    auto source = make_ref<MemorySourceAccessor>();
    auto setContents = [&](std::string contents) {
        source->root = MemorySourceAccessor::File{MemorySourceAccessor::File::Regular{
            .contents = std::move(contents),
        }};
    };
    auto digest = [&](const std::string & selfReference) {
        return hashContentAddress(
            {source, CanonPath::root},
            {.method = ContentAddressMethod::Raw::NixArchive,
             .algorithm = HashAlgorithm::SHA256,
             .selfReference = selfReference});
    };
    setContents("prefix/" + before + "/payload");
    auto original = digest(before);
    ASSERT_TRUE(original.narHashAndSize);
    setContents("prefix/" + after + "/payload");
    auto rewritten = digest(after);
    EXPECT_EQ(original.hash, rewritten.hash);
    ASSERT_TRUE(rewritten.narHashAndSize);
    EXPECT_NE(original.narHashAndSize->hash, rewritten.narHashAndSize->hash);

    // This is the invalid historical producer: it zeroed references but
    // omitted their positions from the CA digest. Its NAR can still be intact.
    setContents("prefix/" + std::string(32, '\0') + "/payload");
    auto zeroed = digest(after);
    EXPECT_NE(original.hash, zeroed.hash);
    EXPECT_EQ(
        zeroed.hash,
        hashPath({source, CanonPath::root}, FileSerialisationMethod::NixArchive, HashAlgorithm::SHA256).hash);
    setContents("prefix/" + after + "/changed");
    EXPECT_NE(original.hash, digest(after).hash);
}

/* ----------------------------------------------------------------------------
 * ContentAddressMethod::parse, ContentAddressMethod::render
 * --------------------------------------------------------------------------*/

static auto methods = ::testing::Values(
    std::pair{ContentAddressMethod::Raw::Text, "text"},
    std::pair{ContentAddressMethod::Raw::Flat, "flat"},
    std::pair{ContentAddressMethod::Raw::NixArchive, "nar"},
    std::pair{ContentAddressMethod::Raw::Git, "git"},
    std::pair{ContentAddressMethod::Raw::JjTree, "jj-tree"});

struct ContentAddressMethodTest : ::testing::Test,
                                  ::testing::WithParamInterface<std::pair<ContentAddressMethod, std::string_view>>
{};

TEST_P(ContentAddressMethodTest, testRoundTripPrintParse_1)
{
    auto & [cam, _] = GetParam();
    EXPECT_EQ(ContentAddressMethod::parse(cam.render()), cam);
}

TEST_P(ContentAddressMethodTest, testRoundTripPrintParse_2)
{
    auto & [cam, camS] = GetParam();
    EXPECT_EQ(ContentAddressMethod::parse(camS).render(), camS);
}

INSTANTIATE_TEST_SUITE_P(ContentAddressMethod, ContentAddressMethodTest, methods);

TEST(ContentAddressMethod, testParseContentAddressMethodOptException)
{
    EXPECT_THROW(ContentAddressMethod::parse("narwhal"), UsageError);
}

/* ----------------------------------------------------------------------------
 * Jujutsu tree ids as content addresses
 * --------------------------------------------------------------------------*/

/* A BLAKE3 id as jj would mint one. Parsed against `nativeIdXpSettings`
   because the tests run without the `blake3-hashes` feature, exactly the
   situation the method has to work in: the algorithm is jj's, not a choice. */
static Hash sampleTreeId()
{
    return Hash::parseSRI("blake3-TAPbLczl0EP0I9SGuLfWmVPhgmOEd9ABjIzodWGd+aA=", nativeIdXpSettings());
}

TEST(ContentAddress, jjTreeRendersAndParsesWithoutBlake3Feature)
{
    ContentAddress ca{.method = ContentAddressMethod::Raw::JjTree, .hash = sampleTreeId()};
    auto rendered = ca.render();
    EXPECT_TRUE(rendered.starts_with("fixed:jj-tree:blake3:")) << rendered;
    EXPECT_EQ(ContentAddress::parse(rendered), ca);
    EXPECT_EQ(ca.printMethodAlgo(), "jj-tree:blake3");
    EXPECT_EQ(
        ContentAddressMethod{ContentAddressMethod::Raw::JjTree}.renderWithAlgo(HashAlgorithm::BLAKE3),
        "fixed:jj-tree:blake3");
    auto [method, algo] = ContentAddressMethod::parseWithAlgo("fixed:jj-tree:blake3");
    EXPECT_EQ(method, ContentAddressMethod::Raw::JjTree);
    EXPECT_EQ(algo, HashAlgorithm::BLAKE3);
}

/* The method fixes the algorithm; a `jj-tree` address over any other hash
   names an object no jj store can serve and is refused at parse time. */
TEST(ContentAddress, jjTreeRefusesOtherAlgorithms)
{
    EXPECT_THAT(
        []() { ContentAddressMethod::parseWithAlgo("fixed:jj-tree:sha256"); },
        testing::ThrowsMessage<UsageError>(testing::HasSubstr("always BLAKE3")));
    EXPECT_THAT(
        []() { ContentAddress::parse("fixed:jj-tree:sha256:1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s"); },
        testing::ThrowsMessage<UsageError>(testing::HasSubstr("always BLAKE3")));
}

/* The id covers the tree's bytes and nothing else. */
TEST(ContentAddress, jjTreeRefusesReferences)
{
    EXPECT_THAT(
        []() {
            ContentAddressWithReferences::fromParts(
                ContentAddressMethod::Raw::JjTree, sampleTreeId(), StoreReferences{.others = {}, .self = true});
        },
        testing::ThrowsMessage<Error>(testing::HasSubstr("cannot refer")));
    auto ca = ContentAddressWithReferences::fromParts(ContentAddressMethod::Raw::JjTree, sampleTreeId(), {});
    EXPECT_EQ(ca.getMethod(), ContentAddressMethod::Raw::JjTree);
    EXPECT_EQ(ca.getHash(), sampleTreeId());
}

/* ----------------------------------------------------------------------------
 * JSON
 * --------------------------------------------------------------------------*/

class ContentAddressTest : public virtual CharacterizationTest
{
    std::filesystem::path unitTestData = getUnitTestData() / "content-address";

public:

    /**
     * We set these in tests rather than the regular globals so we don't have
     * to worry about race conditions if the tests run concurrently.
     */
    ExperimentalFeatureSettings mockXpSettings;

    std::filesystem::path goldenMaster(std::string_view testStem) const override
    {
        return unitTestData / testStem;
    }
};

using nlohmann::json;

struct ContentAddressJsonTest : ContentAddressTest,
                                JsonCharacterizationTest<ContentAddress>,
                                ::testing::WithParamInterface<std::pair<std::string_view, ContentAddress>>
{};

TEST_P(ContentAddressJsonTest, from_json)
{
    auto & [name, expected] = GetParam();
    readJsonTest(name, expected);
}

TEST_P(ContentAddressJsonTest, to_json)
{
    auto & [name, value] = GetParam();
    writeJsonTest(name, value);
}

INSTANTIATE_TEST_SUITE_P(
    ContentAddressJSON,
    ContentAddressJsonTest,
    ::testing::Values(
        std::pair{
            "text",
            ContentAddress{
                .method = ContentAddressMethod::Raw::Text,
                .hash = hashString(HashAlgorithm::SHA256, "asdf"),
            },
        },
        std::pair{
            "nar",
            ContentAddress{
                .method = ContentAddressMethod::Raw::NixArchive,
                .hash = hashString(HashAlgorithm::SHA256, "qwer"),
            },
        },
        std::pair{
            "jj-tree",
            ContentAddress{
                .method = ContentAddressMethod::Raw::JjTree,
                .hash = sampleTreeId(),
            },
        }));

} // namespace nix
