#include "nix/store/local-store.hh"
#include "nix/store/derivations.hh"
#include "nix/store/globals.hh"
#include "nix/store/store-open.hh"
#include "nix/util/file-system.hh"
#include "nix/util/posix-source-accessor.hh"
#include <iostream>
#include <nlohmann/json.hpp>

int main(int argc, char ** argv)
{
    using namespace nix;
    if (argc != 3) return 2;
    initLibStore(false);
    experimentalFeatureSettings.set("extra-experimental-features", "ca-derivations");
    auto store = openStore(argv[1]).dynamic_pointer_cast<LocalStore>();
    if (!store) return 2;
    bool invalid = std::string(argv[2]) == "invalid";
    if (!invalid && std::string(argv[2]) != "valid") return 2;
    Derivation drv;
    drv.name = "remote-retained";
    drv.platform = settings.thisSystem.get();
    drv.builder = "/bin/sh";
    drv.args = {"-c", "printf 'remote payload' > \"$out\""};
    drv.env = {{"name", drv.name}, {"out", hashPlaceholder("out")}, {"system", drv.platform}};
    drv.outputs.emplace("out", DerivationOutput{DerivationOutput::CAFloating{
        .method = ContentAddressMethod::Raw::NixArchive, .hashAlgo = HashAlgorithm::SHA256}});
    auto drvPath = store->writeDerivation(drv);
    auto temporary = std::filesystem::path(argv[1]) / "historical-payload";
    writeFile(temporary, "historical payload");
    auto nar = hashPath(makeFSSourceAccessor(temporary), FileIngestionMethod::NixArchive, HashAlgorithm::SHA256);
    auto address = invalid ? hashString(HashAlgorithm::SHA256, "historical producer identity") : nar.first;
    auto path = store->makeFixedOutputPath(drv.name,
        {.method = FileIngestionMethod::NixArchive, .hash = address, .references = {}});
    std::filesystem::rename(temporary, store->toRealPath(path));
    ValidPathInfo info{path, UnkeyedValidPathInfo(*store, nar.first)};
    if (!nar.second) return 2;
    info.narSize = *nar.second;
    info.ca = ContentAddress{.method = ContentAddressMethod::Raw::NixArchive, .hash = address};
    // Reproduce native metadata admitted by the historical producer. Normal
    // imports correctly refuse this invalid fixture; no live store is used.
    store->registerValidPaths({{path, info}});
    DrvOutput id{.drvHash = staticOutputHashes(*store, drv).at("out"), .outputName = "out"};
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = path}, id});
    std::cout << nlohmann::json{{"drv", store->printStorePath(drvPath)},
        {"old", store->printStorePath(path)}, {"system", drv.platform}} << std::endl;
}
