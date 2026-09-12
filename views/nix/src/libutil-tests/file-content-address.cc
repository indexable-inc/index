#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "nix/util/file-content-address.hh"
#include "nix/util/source-path.hh"

namespace nix {

/* ----------------------------------------------------------------------------
 * parseFileSerialisationMethod, renderFileSerialisationMethod
 * --------------------------------------------------------------------------*/

TEST(FileSerialisationMethod, testRoundTripPrintParse_1)
{
    for (const FileSerialisationMethod fim : {
             FileSerialisationMethod::Flat,
             FileSerialisationMethod::NixArchive,
         }) {
        EXPECT_EQ(parseFileSerialisationMethod(renderFileSerialisationMethod(fim)), fim);
    }
}

TEST(FileSerialisationMethod, testRoundTripPrintParse_2)
{
    for (const std::string_view fimS : {
             "flat",
             "nar",
         }) {
        EXPECT_EQ(renderFileSerialisationMethod(parseFileSerialisationMethod(fimS)), fimS);
    }
}

TEST(FileSerialisationMethod, testParseFileSerialisationMethodOptException)
{
    EXPECT_THAT(
        []() { parseFileSerialisationMethod("narwhal"); },
        testing::ThrowsMessage<UsageError>(testing::HasSubstr("narwhal")));
}

/* ----------------------------------------------------------------------------
 * parseFileIngestionMethod, renderFileIngestionMethod
 * --------------------------------------------------------------------------*/

TEST(FileIngestionMethod, testRoundTripPrintParse_1)
{
    for (const FileIngestionMethod fim : {
             FileIngestionMethod::Flat,
             FileIngestionMethod::NixArchive,
             FileIngestionMethod::Git,
             FileIngestionMethod::JjTree,
         }) {
        EXPECT_EQ(parseFileIngestionMethod(renderFileIngestionMethod(fim)), fim);
    }
}

TEST(FileIngestionMethod, testRoundTripPrintParse_2)
{
    for (const std::string_view fimS : {
             "flat",
             "nar",
             "git",
             "jj-tree",
         }) {
        EXPECT_EQ(renderFileIngestionMethod(parseFileIngestionMethod(fimS)), fimS);
    }
}

/* Nix cannot compute a jj tree id; asking for one is a typed refusal, not a
   walk that fails on the first file or, worse, a hash of the wrong thing. */
TEST(FileIngestionMethod, jjTreeIsNotComputable)
{
    SourcePath root{makeEmptySourceAccessor()};
    EXPECT_THAT(
        [&]() { hashPath(root, FileIngestionMethod::JjTree, HashAlgorithm::BLAKE3); },
        testing::ThrowsMessage<TreeIdNotComputable>(testing::HasSubstr("Jujutsu tree id")));
}

TEST(FileIngestionMethod, testParseFileIngestionMethodOptException)
{
    EXPECT_THAT(
        []() { parseFileIngestionMethod("narwhal"); },
        testing::ThrowsMessage<UsageError>(testing::HasSubstr("narwhal")));
}

} // namespace nix
