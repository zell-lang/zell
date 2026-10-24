/* std_reflect.c - C access to the reflection tables (runtime/ABI.md 17).
 * The tables themselves are emitted by the compiler (codegen/reflect.rs);
 * this file pins their layout and offers lookups for C code. */
#include "rt_internal.h"

#include <stddef.h>
#include <string.h>

_Static_assert(offsetof(zell_reflect_type, kind) == 24, "TypeInfo.Kind");
_Static_assert(offsetof(zell_reflect_type, name) == 32, "TypeInfo.Name");
_Static_assert(offsetof(zell_reflect_type, fields) == 64, "TypeInfo.Fields");
_Static_assert(offsetof(zell_reflect_type, methods) == 96, "TypeInfo.Methods");
_Static_assert(offsetof(zell_reflect_type, variants) == 128, "TypeInfo.Variants");
_Static_assert(offsetof(zell_reflect_type, elem) == 160, "TypeInfo.Elem");
_Static_assert(offsetof(zell_reflect_type, attrs) == 184, "TypeInfo.attrs");
_Static_assert(offsetof(zell_reflect_type, ptr_to) == 216, "TypeInfo.ptrTo");
_Static_assert(offsetof(zell_reflect_type, package) == 232, "TypeInfo.Package");
_Static_assert(sizeof(zell_reflect_type) == 240, "TypeInfo size");
_Static_assert(offsetof(zell_reflect_field, attrs) == 40 && sizeof(zell_reflect_field) == 72, "FieldInfo");
_Static_assert(offsetof(zell_reflect_method, invoke) == 24 && offsetof(zell_reflect_method, params) == 72 && sizeof(zell_reflect_method) == 104, "MethodInfo");
_Static_assert(offsetof(zell_reflect_param, attrs) == 24 && sizeof(zell_reflect_param) == 56, "ParamInfo");
_Static_assert(offsetof(zell_reflect_variant, fields) == 16 && sizeof(zell_reflect_variant) == 80, "VariantInfo");
_Static_assert(sizeof(zell_reflect_any) == 16, "any");

static const char *const KIND_NAMES[ZELL_KIND_COUNT] = {
    "Invalid", "Void", "Bool", "Int", "Int8", "Int16", "Int32", "Uint", "Uint8", "Uint16",
    "Uint32", "Float", "Float32", "Char", "String", "Error", "Any", "Interface", "Struct", "Enum",
    "Pointer", "Weak", "Slice", "Array", "Map", "Tuple", "Func", "Fallible", "Task",
};

const zell_reflect_type *zell_reflect_type_of(const void *obj) {
    if (!obj) return NULL;
    return (const zell_reflect_type *)((const zell_object *)obj)->type;
}

const char *zell_reflect_kind_name(int32_t kind) {
    if (kind < 0 || kind >= ZELL_KIND_COUNT) return "Invalid";
    return KIND_NAMES[kind];
}

static bool name_is(const zell_string *s, const char *name) {
    size_t n = strlen(name);
    return (s ? (size_t)s->len : 0) == n && memcmp(s ? s->data : "", name, n) == 0;
}

const zell_reflect_field *zell_reflect_field_by_name(const zell_reflect_type *t, const char *name) {
    if (!t || !name) return NULL;
    const zell_reflect_field *const *fs = t->fields.data;
    for (int64_t i = 0; i < t->fields.len; i++) {
        if (name_is(fs[i]->name, name)) return fs[i];
    }
    return NULL;
}

const zell_reflect_method *zell_reflect_method_by_name(const zell_reflect_type *t, const char *name) {
    if (!t || !name) return NULL;
    const zell_reflect_method *const *ms = t->methods.data;
    for (int64_t i = 0; i < t->methods.len; i++) {
        if (name_is(ms[i]->name, name)) return ms[i];
    }
    return NULL;
}

const zell_reflect_type *zell_reflect_canon(const zell_reflect_type *t) {
    return t && t->canon ? t->canon : t;
}
