#include "nix/fetchers/fetch-settings.hh"
#include "nix/fetchers/registry.hh"
#include "nix/fetchers/tarball.hh"
#include "nix/util/users.hh"
#include "nix/store/global-paths.hh"
#include "nix/store/store-api.hh"
#include "nix/store/local-fs-store.hh"
#include "ixe-fetch-registry.h"

namespace nix::fetchers {

namespace {

void checkRegistry(char * error)
{
    if (error) {
        std::unique_ptr<char, decltype(&ixe_registry_string_free)> owned(error, ixe_registry_string_free);
        throw Error("%s", owned.get());
    }
}

IxeRegistryBytes bytes(std::string_view value)
{
    return {reinterpret_cast<const uint8_t *>(value.data()), value.size()};
}

std::string copyBytes(IxeRegistryBytes value)
{
    return {reinterpret_cast<const char *>(value.data), value.len};
}

/** Borrows the attributes for one synchronous call. */
struct AttrView
{
    std::vector<IxeRegistryAttr> attributes;

    explicit AttrView(const Attrs & attrs)
    {
        attributes.reserve(attrs.size());
        for (auto & [name, value] : attrs) {
            IxeRegistryAttr attr{.name = bytes(name), .kind = 0, .string = bytes(""), .number = 0};
            if (auto string = std::get_if<std::string>(&value))
                attr.string = bytes(*string);
            else if (auto integer = std::get_if<uint64_t>(&value)) {
                attr.kind = 1;
                attr.number = *integer;
            } else {
                attr.kind = 2;
                attr.number = std::get<Explicit<bool>>(value).t;
            }
            attributes.push_back(attr);
        }
    }

    IxeRegistryAttrsView view() const
    {
        return {attributes.data(), attributes.size()};
    }
};

Attrs copyAttrs(IxeRegistryAttrsView view)
{
    Attrs result;
    for (size_t index = 0; index < view.len; ++index) {
        auto & attr = view.data[index];
        auto name = copyBytes(attr.name);
        switch (attr.kind) {
        case 0:
            result.emplace(std::move(name), copyBytes(attr.string));
            break;
        case 1:
            result.emplace(std::move(name), attr.number);
            break;
        case 2:
            result.emplace(std::move(name), Explicit<bool>{attr.number != 0});
            break;
        default:
            throw Error("invalid attribute tag returned by Rust registry");
        }
    }
    return result;
}

/** Reattach the fetch implementation to attributes already owned by Rust.
 * Registry::read validates/normalizes each backend before publishing the owner.
 * add() receives an Input which its caller has already constructed. */
Input attachInput(Attrs attrs)
{
    Input input;
    input.attrs = std::move(attrs);
    auto & schemes = getAllInputSchemes();
    auto scheme = schemes.find(input.getType());
    if (scheme != schemes.end())
        input.scheme = scheme->second;
    return input;
}

struct EntryView
{
    AttrView from, to, extra;
    bool exact;

    explicit EntryView(const Registry::Entry & entry)
        : from(entry.from.attrs)
        , to(entry.to.attrs)
        , extra(entry.extraAttrs)
        , exact(entry.exact)
    {
    }

    IxeRegistryEntryView view() const
    {
        return {from.view(), to.view(), extra.view(), static_cast<uint8_t>(exact)};
    }
};

} // namespace

struct Registry::Impl
{
    std::unique_ptr<IxeRegistry, decltype(&ixe_registry_free)> handle{nullptr, ixe_registry_free};

    Impl()
    {
        IxeRegistry * owner = nullptr;
        checkRegistry(ixe_registry_new(&owner));
        handle.reset(owner);
    }
};

Registry::Registry(RegistryType type)
    : type(type)
    , impl(std::make_unique<Impl>())
{
}

Registry::~Registry() = default;

std::vector<Registry::Entry> Registry::entries() const
{
    IxeRegistryEntries * raw = nullptr;
    checkRegistry(ixe_registry_entries(impl->handle.get(), &raw));
    std::unique_ptr<IxeRegistryEntries, decltype(&ixe_registry_entries_free)> snapshot(raw, ixe_registry_entries_free);
    std::vector<Entry> result;
    auto count = ixe_registry_entries_len(snapshot.get());
    result.reserve(count);
    for (size_t index = 0; index < count; ++index) {
        IxeRegistryEntryView entry;
        checkRegistry(ixe_registry_entries_get(snapshot.get(), index, &entry));
        result.push_back({
            .from = attachInput(copyAttrs(entry.from)),
            .to = attachInput(copyAttrs(entry.to)),
            .extraAttrs = copyAttrs(entry.extra),
            .exact = entry.exact != 0,
        });
    }
    return result;
}

std::shared_ptr<Registry> Registry::read(const Settings & settings, const SourcePath & path, RegistryType type)
{
    debug("reading registry '%s'", path);
    auto registry = std::make_shared<Registry>(type);
    if (!path.pathExists())
        return registry;

    auto source = path.readFile();
    IxeRegistry * parsed = nullptr;
    checkRegistry(ixe_registry_parse(bytes(source), &parsed));
    registry->impl->handle.reset(parsed);

    // Backend construction can rewrite an input (for example forge archives
    // with submodules become Git inputs). Publish all normalized entries in
    // one transaction; a malformed entry never leaves a partial registry.
    auto entries = registry->entries();
    std::vector<EntryView> transports;
    transports.reserve(entries.size());
    for (auto & entry : entries) {
        entry.from = Input::fromAttrs(settings, std::move(entry.from.attrs));
        entry.to = Input::fromAttrs(settings, std::move(entry.to.attrs));
        transports.emplace_back(entry);
    }
    std::vector<IxeRegistryEntryView> views;
    views.reserve(transports.size());
    for (auto & transport : transports)
        views.push_back(transport.view());
    checkRegistry(ixe_registry_replace(registry->impl->handle.get(), views.data(), views.size()));
    return registry;
}

void Registry::write(const std::filesystem::path & path)
{
    char * raw = nullptr;
    checkRegistry(ixe_registry_serialize(impl->handle.get(), &raw));
    std::unique_ptr<char, decltype(&ixe_registry_string_free)> source(raw, ixe_registry_string_free);
    createDirs(path.parent_path());
    writeFile(path, source.get());
}

void Registry::add(const Input & from, const Input & to, const Attrs & extraAttrs)
{
    AttrView fromView(from.attrs), toView(to.attrs), extraView(extraAttrs);
    checkRegistry(ixe_registry_add(impl->handle.get(), {fromView.view(), toView.view(), extraView.view(), 0}));
}

void Registry::remove(const Input & input)
{
    AttrView view(input.attrs);
    checkRegistry(ixe_registry_remove(impl->handle.get(), view.view()));
}

Input Input::applyOverrides(std::optional<std::string> ref, std::optional<Hash> rev) const
{
    AttrView view(attrs);
    auto revision = rev ? rev->gitRev() : std::string();
    IxeRegistryAttrs * raw = nullptr;
    checkRegistry(ixe_registry_apply_overrides(
        view.view(),
        ref.has_value(),
        bytes(ref ? std::string_view(*ref) : std::string_view()),
        rev.has_value(),
        bytes(revision),
        &raw));
    std::unique_ptr<IxeRegistryAttrs, decltype(&ixe_registry_attrs_free)> result(raw, ixe_registry_attrs_free);
    IxeRegistryAttrsView resultView;
    checkRegistry(ixe_registry_attrs_view(result.get(), &resultView));
    auto input(*this);
    input.attrs = copyAttrs(resultView);
    input.cachedFingerprint.reset();
    return input;
}

static std::shared_ptr<Registry> getSystemRegistry(const Settings & settings)
{
    return Registry::read(
        settings,
        SourcePath{getFSSourceAccessor(), CanonPath{(nixConfDir() / "registry.json").string()}}.resolveSymlinks(),
        Registry::System);
}

std::filesystem::path getUserRegistryPath()
{
    return getConfigDir() / "registry.json";
}

std::shared_ptr<Registry> getUserRegistry(const Settings & settings)
{
    return Registry::read(
        settings,
        SourcePath{getFSSourceAccessor(), CanonPath{getUserRegistryPath().string()}}.resolveSymlinks(),
        Registry::User);
}

std::shared_ptr<Registry> getCustomRegistry(const Settings & settings, const std::filesystem::path & path)
{
    return Registry::read(
        settings, SourcePath{getFSSourceAccessor(), CanonPath{path.string()}}.resolveSymlinks(), Registry::Custom);
}

static std::shared_ptr<Registry> getFlagRegistry()
{
    static auto registry = std::make_shared<Registry>(Registry::Flag);
    return registry;
}

void overrideRegistry(const Input & from, const Input & to, const Attrs & extraAttrs)
{
    getFlagRegistry()->add(from, to, extraAttrs);
}

static std::shared_ptr<Registry> getGlobalRegistry(const Settings & settings, Store & store)
{
    auto path = settings.flakeRegistry.get();
    if (path.empty())
        return std::make_shared<Registry>(Registry::Global);

    return Registry::read(
        settings,
        [&]() -> SourcePath {
            std::filesystem::path file{path};
            if (file.is_absolute())
                return SourcePath{getFSSourceAccessor(), CanonPath{file.string()}}.resolveSymlinks();
            auto storePath = downloadFile(store, settings, path, "flake-registry.json").storePath;
            if (auto local = dynamic_cast<LocalFSStore *>(&store))
                local->addPermRoot(storePath, (getCacheDir() / "flake-registry.json").string());
            return {store.requireStoreObjectAccessor(storePath)};
        }(),
        Registry::Global);
}

Registries getRegistries(const Settings & settings, Store & store)
{
    // Reload file-backed layers for each request, including changed paths.
    return {
        getFlagRegistry(), getUserRegistry(settings), getSystemRegistry(settings), getGlobalRegistry(settings, store)};
}

std::pair<Input, Attrs>
lookupInRegistries(const Settings & settings, Store & store, const Input & input, UseRegistries useRegistries)
{
    auto registries = useRegistries == UseRegistries::No ? Registries{} : getRegistries(settings, store);
    std::vector<IxeRegistryLayer> layers;
    for (auto & registry : registries)
        layers.push_back({registry->impl->handle.get(), static_cast<uint8_t>(registry->type)});
    AttrView inputView(input.attrs);
    IxeRegistryResolution * raw = nullptr;
    checkRegistry(ixe_registry_resolve(
        layers.data(), layers.size(), inputView.view(), static_cast<uint8_t>(useRegistries), &raw));
    std::unique_ptr<IxeRegistryResolution, decltype(&ixe_registry_resolution_free)> result(
        raw, ixe_registry_resolution_free);
    IxeRegistryAttrsView resolved, extra;
    checkRegistry(ixe_registry_resolution_input(result.get(), &resolved));
    checkRegistry(ixe_registry_resolution_extra(result.get(), &extra));
    return {attachInput(copyAttrs(resolved)), copyAttrs(extra)};
}

} // namespace nix::fetchers
