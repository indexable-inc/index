#ifndef IXE_FETCH_REGISTRY_H
#define IXE_FETCH_REGISTRY_H
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif

typedef struct IxeRegistry IxeRegistry;
typedef struct IxeRegistryEntries IxeRegistryEntries;
typedef struct IxeRegistryAttrs IxeRegistryAttrs;
typedef struct IxeRegistryResolution IxeRegistryResolution;

typedef struct
{
    const uint8_t * data;
    size_t len;
} IxeRegistryBytes;

typedef struct
{
    IxeRegistryBytes name;
    uint8_t kind; /* 0 = string, 1 = unsigned integer, 2 = boolean */
    IxeRegistryBytes string;
    uint64_t number; /* boolean values must be 0 or 1 */
} IxeRegistryAttr;

typedef struct
{
    const IxeRegistryAttr * data;
    size_t len;
} IxeRegistryAttrsView;

typedef struct
{
    IxeRegistryAttrsView from;
    IxeRegistryAttrsView to;
    IxeRegistryAttrsView extra;
    uint8_t exact;
} IxeRegistryEntryView;

typedef struct
{
    const IxeRegistry * registry;
    uint8_t kind; /* 0 = flag, 1 = user, 2 = system, 3 = global, 4 = custom */
} IxeRegistryLayer;

/* Fallible calls return NULL on success or an owned error string. Outputs
 * are written only on success. Handles have independent ownership; views
 * borrow their snapshot until its free function. Registry methods may run
 * concurrently, but freeing a handle requires exclusive ownership. */
char * ixe_registry_new(IxeRegistry ** out);
char * ixe_registry_parse(IxeRegistryBytes source, IxeRegistry ** out);
char * ixe_registry_serialize(const IxeRegistry * registry, char ** out);
char * ixe_registry_entries(const IxeRegistry * registry, IxeRegistryEntries ** out);
size_t ixe_registry_entries_len(const IxeRegistryEntries * entries);
char * ixe_registry_entries_get(const IxeRegistryEntries * entries, size_t index, IxeRegistryEntryView * out);
char * ixe_registry_replace(const IxeRegistry * registry, const IxeRegistryEntryView * entries, size_t len);
char * ixe_registry_add(const IxeRegistry * registry, IxeRegistryEntryView entry);
char * ixe_registry_remove(const IxeRegistry * registry, IxeRegistryAttrsView input);
/* mode: 0 = no registries, 1 = all layers, 2 = flag/global layers only. */
char * ixe_registry_resolve(
    const IxeRegistryLayer * layers,
    size_t len,
    IxeRegistryAttrsView input,
    uint8_t mode,
    IxeRegistryResolution ** out);
char * ixe_registry_resolution_input(const IxeRegistryResolution * result, IxeRegistryAttrsView * out);
char * ixe_registry_resolution_extra(const IxeRegistryResolution * result, IxeRegistryAttrsView * out);
char * ixe_registry_apply_overrides(
    IxeRegistryAttrsView input,
    uint8_t has_ref,
    IxeRegistryBytes ref,
    uint8_t has_rev,
    IxeRegistryBytes rev,
    IxeRegistryAttrs ** out);
char * ixe_registry_attrs_view(const IxeRegistryAttrs * attrs, IxeRegistryAttrsView * out);
void ixe_registry_free(IxeRegistry * registry);
void ixe_registry_entries_free(IxeRegistryEntries * entries);
void ixe_registry_resolution_free(IxeRegistryResolution * result);
void ixe_registry_attrs_free(IxeRegistryAttrs * attrs);
void ixe_registry_string_free(char * text);

#ifdef __cplusplus
}
#endif
#endif
