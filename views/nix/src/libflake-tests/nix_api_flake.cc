#include <gtest/gtest.h>
#include <filesystem>
#include <string>

#include "nix/util/file-system.hh"
#include "nix_api_store.h"
#include "nix_api_util.h"
#include "nix_api_flake.h"
#include "nix/util/tests/string_callback.hh"
#include "nix/store/tests/nix_api_store.hh"
#include "nix_api_fetchers.h"

namespace nixC {

TEST_F(nix_api_store_test, nix_api_flake_reference_not_absolute_no_basedir_fail)
{
    nix_libstore_init(ctx);
    assert_ctx_ok();

    auto fetchSettings = nix_fetchers_settings_new(ctx);
    assert_ctx_ok();
    ASSERT_NE(nullptr, fetchSettings);

    auto parseFlags = nix_flake_reference_parse_flags_new(ctx);
    assert_ctx_ok();
    ASSERT_NE(nullptr, parseFlags);

    std::string str(".#legacyPackages.aarch127-unknown...orion");
    std::string fragment;
    nix_flake_reference * flakeReference = nullptr;
    auto r = nix_flake_reference_and_fragment_from_string(
        ctx, fetchSettings, parseFlags, str.data(), str.size(), &flakeReference, OBSERVE_STRING(fragment));

    ASSERT_NE(NIX_OK, r);
    ASSERT_EQ(nullptr, flakeReference);

    nix_flake_reference_parse_flags_free(parseFlags);
    nix_fetchers_settings_free(fetchSettings);
}

TEST_F(nix_api_store_test, nix_api_flake_reference_relative_with_fragment)
{
    auto tmpDir = nix::createTempDir();
    nix::AutoDelete delTmpDir(tmpDir, true);

    nix::writeFile(tmpDir / "flake.nix", R"(
        {
            outputs = { ... }: {
                hello = "potato";
            };
        }
    )");

    nix_libstore_init(ctx);
    assert_ctx_ok();

    auto fetchSettings = nix_fetchers_settings_new(ctx);
    assert_ctx_ok();
    ASSERT_NE(nullptr, fetchSettings);

    auto parseFlags = nix_flake_reference_parse_flags_new(ctx);
    assert_ctx_ok();
    ASSERT_NE(nullptr, parseFlags);

    auto r0 = nix_flake_reference_parse_flags_set_base_directory(
        ctx, parseFlags, tmpDir.string().c_str(), tmpDir.string().size());
    assert_ctx_ok();
    ASSERT_EQ(NIX_OK, r0);

    std::string fragment;
    const std::string ref = ".#legacyPackages.aarch127-unknown...orion";
    nix_flake_reference * flakeReference = nullptr;
    auto r = nix_flake_reference_and_fragment_from_string(
        ctx, fetchSettings, parseFlags, ref.data(), ref.size(), &flakeReference, OBSERVE_STRING(fragment));
    assert_ctx_ok();
    ASSERT_EQ(NIX_OK, r);
    ASSERT_NE(nullptr, flakeReference);
    ASSERT_EQ(fragment, "legacyPackages.aarch127-unknown...orion");

    nix_flake_reference_parse_flags_free(parseFlags);

    nix_flake_reference_free(flakeReference);
    nix_fetchers_settings_free(fetchSettings);
}

} // namespace nixC
