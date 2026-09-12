#include "nix/util/config-global.hh"

using namespace nix;

struct MySettings : Config
{
    Setting<bool> settingSet{this, false, "setting-set", "Whether the plugin-defined setting was set"};
};

MySettings mySettings;

static GlobalConfig::Register rs(&mySettings);
