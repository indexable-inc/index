#include <nlohmann/json.hpp>
#include <gtest/gtest.h>

#include "nix/store/path-info.hh"

#include "nix/util/tests/characterization.hh"
#include "nix/store/tests/libstore.hh"

namespace nix {

using nlohmann::json;

class PathInfoTestV1 : public CharacterizationTest, public LibStoreTest
{
    std::filesystem::path unitTestData = getUnitTestData() / "path-info" / "json-1";

    std::filesystem::path goldenMaster(std::string_view testStem) const override
    {
        return unitTestData / (testStem + ".json");
    }
};

class PathInfoTestV2 : public CharacterizationTest, public LibStoreTest
{
    std::filesystem::path unitTestData = getUnitTestData() / "path-info" / "json-2";

    std::filesystem::path goldenMaster(std::string_view testStem) const override
    {
        return unitTestData / (testStem + ".json");
    }
};

static UnkeyedValidPathInfo makeEmpty()
{
    return {
        "/nix/store",
        Hash::parseSRI("sha256-FePFYIlMuycIXPZbWi7LGEiMmZSX9FMbaQenWBzm1Sc="),
    };
}

/* ----------------------------------------------------------------------------
 * Jujutsu tree objects: addressed by an id Nix cannot recompute
 * --------------------------------------------------------------------------*/

class JjTreePathInfoTest : public LibStoreTest
{
protected:
    Hash treeId = Hash::parseSRI("blake3-TAPbLczl0EP0I9SGuLfWmVPhgmOEd9ABjIzodWGd+aA=", nativeIdXpSettings());
};

/* The whole read path: name plus id gives the store path, no bytes involved,
   and it is the `output:out` scheme over `fixed:out:jj-tree:blake3:<hex>:`
   (the algorithm is part of the payload, `store-dir-config.cc`) as the store
   path specification says. */
TEST_F(JjTreePathInfoTest, storePathFromIdAlone)
{
    auto path = store->makeFixedOutputPath(
        "source", FixedOutputInfo{.method = FileIngestionMethod::JjTree, .hash = treeId, .references = {}});
    auto expected = store->makeStorePath(
        "output:out",
        hashString(HashAlgorithm::SHA256, "fixed:out:jj-tree:" + treeId.to_string(HashFormat::Base16, true) + ":"),
        "source");
    EXPECT_EQ(path, expected);
    EXPECT_EQ(
        store->makeFixedOutputPathFromCA(
            "source", ContentAddressWithReferences::fromParts(ContentAddressMethod::Raw::JjTree, treeId, {})),
        expected);
}

TEST_F(JjTreePathInfoTest, storePathRefusesNonBlake3)
{
    EXPECT_THAT(
        [&]() {
            store->makeFixedOutputPath(
                "source",
                FixedOutputInfo{
                    .method = FileIngestionMethod::JjTree,
                    .hash = hashString(HashAlgorithm::SHA256, "not a tree id"),
                    .references = {}});
        },
        testing::ThrowsMessage<Error>(testing::HasSubstr("BLAKE3")));
}

/* Content-addressed, yes: the path is the one the ca derives. Self-certifying,
   no: nothing in Nix can check the id against the bytes, so without a
   signature the object counts zero signatures, like an input-addressed path. */
TEST_F(JjTreePathInfoTest, notSelfCertifying)
{
    auto info = ValidPathInfo::makeFromCA(
        *store,
        "source",
        ContentAddressWithReferences::fromParts(ContentAddressMethod::Raw::JjTree, treeId, {}),
        Hash::parseSRI("sha256-FePFYIlMuycIXPZbWi7LGEiMmZSX9FMbaQenWBzm1Sc="));
    EXPECT_TRUE(info.isContentAddressed(*store));
    EXPECT_FALSE(info.isSelfCertifying(*store));
    EXPECT_EQ(info.checkSignatures(*store, PublicKeys{}), 0u);

    /* Control: the same shape under NAR addressing is self-certifying. */
    auto narInfo = ValidPathInfo::makeFromCA(
        *store,
        "source",
        ContentAddressWithReferences::fromParts(
            ContentAddressMethod::Raw::NixArchive, hashString(HashAlgorithm::SHA256, "bytes"), {}),
        hashString(HashAlgorithm::SHA256, "bytes"));
    EXPECT_TRUE(narInfo.isSelfCertifying(*store));
    EXPECT_EQ(narInfo.checkSignatures(*store, PublicKeys{}), ValidPathInfo::maxSigs);
}

static ValidPathInfo makeFullKeyed(const Store & store, bool includeImpureInfo)
{
    auto info = ValidPathInfo::makeFromCA(
        store,
        "foo",
        FixedOutputInfo{
            .method = FileIngestionMethod::NixArchive,
            .hash = hashString(HashAlgorithm::SHA256, "(...)"),

            .references =
                {
                    .others =
                        {
                            StorePath{
                                "g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-bar",
                            },
                        },
                    .self = true,
                },
        },
        Hash::parseSRI("sha256-FePFYIlMuycIXPZbWi7LGEiMmZSX9FMbaQenWBzm1Sc="));
    info.narSize = 34878;
    if (includeImpureInfo) {
        info.deriver = StorePath{
            "g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-bar.drv",
        };
        info.registrationTime = 23423;
        info.ultimate = true;
        info.sigs = {
            Signature{.keyName = "asdf", .sig = std::string(64, '\0')},
            Signature{.keyName = "qwer", .sig = std::string(64, '\0')},
        };
    }
    return info;
}

static UnkeyedValidPathInfo makeFull(const Store & store, bool includeImpureInfo)
{
    return makeFullKeyed(store, includeImpureInfo);
}

#define JSON_READ_TEST_V1(STEM, OBJ)                                                     \
    TEST_F(PathInfoTestV1, PathInfo_##STEM##_from_json)                                  \
    {                                                                                    \
        readTest(#STEM, [&](const auto & encoded_) {                                     \
            auto encoded = json::parse(encoded_);                                        \
            UnkeyedValidPathInfo got = UnkeyedValidPathInfo::fromJSON(&*store, encoded); \
            auto expected = OBJ;                                                         \
            ASSERT_EQ(got, expected);                                                    \
        });                                                                              \
    }

#define JSON_WRITE_TEST_V1(STEM, OBJ, PURE)                                                           \
    TEST_F(PathInfoTestV1, PathInfo_##STEM##_to_json)                                                 \
    {                                                                                                 \
        writeTest(                                                                                    \
            #STEM,                                                                                    \
            [&]() -> json { return OBJ.toJSON(&*store, PURE, PathInfoJsonFormat::V1); },              \
            [](const auto & file) { return json::parse(readFile(file)); },                            \
            [](const auto & file, const auto & got) { return writeFile(file, got.dump(2) + "\n"); }); \
    }

#define JSON_TEST_V1(STEM, OBJ, PURE) \
    JSON_READ_TEST_V1(STEM, OBJ)      \
    JSON_WRITE_TEST_V1(STEM, OBJ, PURE)

#define JSON_READ_TEST_V2(STEM, OBJ)                                                     \
    TEST_F(PathInfoTestV2, PathInfo_##STEM##_from_json)                                  \
    {                                                                                    \
        readTest(#STEM, [&](const auto & encoded_) {                                     \
            auto encoded = json::parse(encoded_);                                        \
            UnkeyedValidPathInfo got = UnkeyedValidPathInfo::fromJSON(nullptr, encoded); \
            auto expected = OBJ;                                                         \
            ASSERT_EQ(got, expected);                                                    \
        });                                                                              \
    }

#define JSON_WRITE_TEST_V2(STEM, OBJ, PURE)                                                           \
    TEST_F(PathInfoTestV2, PathInfo_##STEM##_to_json)                                                 \
    {                                                                                                 \
        writeTest(                                                                                    \
            #STEM,                                                                                    \
            [&]() -> json { return OBJ.toJSON(nullptr, PURE, PathInfoJsonFormat::V2); },              \
            [](const auto & file) { return json::parse(readFile(file)); },                            \
            [](const auto & file, const auto & got) { return writeFile(file, got.dump(2) + "\n"); }); \
    }

#define JSON_TEST_V2(STEM, OBJ, PURE) \
    JSON_READ_TEST_V2(STEM, OBJ)      \
    JSON_WRITE_TEST_V2(STEM, OBJ, PURE)

JSON_TEST_V1(empty_pure, makeEmpty(), false)
JSON_TEST_V1(empty_impure, makeEmpty(), true)
JSON_TEST_V1(pure, makeFull(*store, false), false)
JSON_TEST_V1(impure, makeFull(*store, true), true)

// Test that JSON without explicit version field parses as V1
JSON_READ_TEST_V1(pure_noversion, makeFull(*store, false))

JSON_TEST_V2(empty_pure, makeEmpty(), false)
JSON_TEST_V2(empty_impure, makeEmpty(), true)
JSON_TEST_V2(pure, makeFull(*store, false), false)
JSON_TEST_V2(impure, makeFull(*store, true), true)

TEST_F(PathInfoTestV2, PathInfo_full_shortRefs)
{
    ValidPathInfo it = makeFullKeyed(*store, true);
    // it.references = unkeyed.references;
    auto refs = it.shortRefs();
    ASSERT_EQ(refs.size(), 2u);
    ASSERT_EQ(*refs.begin(), "g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-bar");
    ASSERT_EQ(*++refs.begin(), "n5wkd9frr45pa74if5gpz9j7mifg27fh-foo");
}

} // namespace nix
