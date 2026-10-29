/* The things Rust would otherwise have to mirror flecs's large structs for: setting the OS
 * API hooks (ecs_os_api_t), listing children and roots (ecs_iter_t, ecs_query_desc_t), and the sizes of the structs Rust does
 * mirror, for a test that compares them on every target. */
#include <float.h>
#include <stddef.h>
#include "flecs.h"

typedef void (*glue_log_t)(int32_t level, const char *file, int32_t line, const char *msg);

static void glue_zero_time(ecs_time_t *t) {
    t->sec = 0;
    t->nanosec = 0;
}

static uint64_t glue_zero_now(void) { return 0; }

static void glue_sleep(int32_t sec, int32_t nanosec) {
    (void)sec;
    (void)nanosec;
}

/* Flecs's defaults (its own malloc wrappers over the C library), with logging sent to `log`,
 * abort sent to `abort_fn`, and every clock reading 0: nothing here may depend on the time, so
 * the same scripts give the same world everywhere. Call before the first ecs_init. */
void glue_os_api(glue_log_t log, void (*abort_fn)(void)) {
    ecs_os_set_api_defaults();
    ecs_os_api_t api = ecs_os_get_api();
    api.log_ = log;
    api.abort_ = abort_fn;
#ifndef GLUE_REST
    /* With REST the HTTP layer's request cache reads the clock to expire entries; nothing else does. */
    api.get_time_ = glue_zero_time;
    api.now_ = glue_zero_now;
#else
    (void)glue_zero_time;
    (void)glue_zero_now;
#endif
    api.sleep_ = glue_sleep;
    api.flags_ &= ~(ecs_flags32_t)EcsOsApiLogWithColors;
    ecs_os_set_api(&api);
}

/* The children of `parent` (ChildOf, either storage), in flecs's order: the declared order when
 * the parent has OrderedChildren, otherwise table order. Writes up to `cap` ids to `out` and
 * returns how many there are, so a caller with too small a buffer calls again. */
int32_t glue_children(const ecs_world_t *world, ecs_entity_t parent, ecs_entity_t *out, int32_t cap) {
    int32_t n = 0;
    ecs_iter_t it = ecs_children(world, parent);
    while (ecs_children_next(&it)) {
        for (int32_t i = 0; i < it.count; i++) {
            if (n < cap) out[n] = it.entities[i];
            n++;
        }
    }
    return n;
}

/* Every named entity without a parent, prefabs and disabled entities included (a raw walk of the
 * tables with a name, so no query features are needed). Same contract as glue_children. */
int32_t glue_roots(ecs_world_t *world, ecs_entity_t *out, int32_t cap) {
    int32_t n = 0;
    ecs_iter_t it = ecs_each_id(world, ecs_pair(ecs_id(EcsIdentifier), EcsName));
    while (ecs_each_next(&it)) {
        for (int32_t i = 0; i < it.count; i++) {
            if (ecs_get_parent(world, it.entities[i])) continue;
            if (n < cap) out[n] = it.entities[i];
            n++;
        }
    }
    return n;
}

/* Every entity whose type has `id` (a raw walk of the tables, like glue_roots). Same contract as glue_children. */
int32_t glue_with(const ecs_world_t *world, ecs_id_t id, ecs_entity_t *out, int32_t cap) {
    int32_t n = 0;
    ecs_iter_t it = ecs_each_id(world, id);
    while (ecs_each_next(&it)) {
        for (int32_t i = 0; i < it.count; i++) {
            if (n < cap) out[n] = it.entities[i];
            n++;
        }
    }
    return n;
}

/* Flecs past 4.1.6 (this repo's pin, vendor/flecs/SOURCE.md) has no defer mode: operations queue only on a stage and
 * apply when it is merged, and ecs_is_deferred, ecs_defer_begin and ecs_defer_end are gone. A host's Flecs (the space
 * game's 4.1.6, SPEC.md "Host worlds") still has them. EcsIdOnInstantiateCopy is new with the rename of
 * (OnInstantiate, Override) to (OnInstantiate, Copy), after the defer API went, so it tells the two apart. */
#ifdef EcsIdOnInstantiateCopy
#define GLUE_NO_DEFER
#endif

/* Whether `world` cannot be written straight away: a stage, or a world in readonly mode (a system or observer is
 * running), or, on 4.1.6, a world in defer mode. */
bool glue_is_deferred(const ecs_world_t *world) {
#ifdef GLUE_NO_DEFER
    return ecs_is_stage(world) || ecs_stage_is_readonly(world);
#else
    return ecs_is_deferred(world);
#endif
}

/* Starts a batch of writes that apply together at glue_batch_end, so an entity moves to its final table once. Returns
 * the world to write the batch to (stage 0 past 4.1.6; the world itself, deferred, on 4.1.6). Reads go to `world`. */
ecs_world_t *glue_batch_begin(ecs_world_t *world) {
#ifdef GLUE_NO_DEFER
    return ecs_get_stage(world, 0);
#else
    ecs_defer_begin(world);
    return world;
#endif
}

/* Applies the batch glue_batch_begin returned. */
void glue_batch_end(ecs_world_t *world, ecs_world_t *batch) {
#ifdef GLUE_NO_DEFER
    (void)world;
    ecs_merge(batch);
#else
    (void)batch;
    ecs_defer_end(world);
#endif
}

/* A value being written from Rust: a default-constructed value of one type (ecs_value_new, so its hooks run) and a
 * meta cursor over it. Rust walks the cursor (ecs_meta_push, ecs_meta_member, ecs_meta_set_*), then copies the value
 * onto an entity with glue_writer_commit, then frees it. The struct lives here because ecs_meta_cursor_t is large and
 * returned by value. */
typedef struct glue_writer_t {
    ecs_value_t value;
    ecs_meta_cursor_t cursor;
} glue_writer_t;

glue_writer_t *glue_writer_new(const ecs_world_t *world, ecs_entity_t type) {
    glue_writer_t *w = ecs_os_calloc_t(glue_writer_t);
    if (!w) return NULL;
    w->value = ecs_value_new(world, type);
    if (!w->value.ptr) {
        ecs_os_free(w);
        return NULL;
    }
    w->cursor = ecs_meta_cursor(world, type, w->value.ptr);
    return w;
}

ecs_meta_cursor_t *glue_writer_cursor(glue_writer_t *w) { return &w->cursor; }

/* The value being written (a default-constructed one until the cursor writes it). */
const void *glue_writer_ptr(glue_writer_t *w) { return w->value.ptr; }

/* Copies the value onto `entity` as `component` (the type's copy hook runs). */
void glue_writer_commit(ecs_world_t *world, glue_writer_t *w, ecs_entity_t entity, ecs_id_t component, size_t size) {
    ecs_set_id(world, entity, component, size, w->value.ptr);
}

void glue_writer_free(const ecs_world_t *world, glue_writer_t *w) {
    if (!w) return;
    ecs_value_fini(world, &w->value);
    ecs_os_free(w);
}

/* Flecs's per-table change counters (flecs.c, not in flecs.h): one int32 for the entity column and one per data column,
 * bumped by set, modified and every query that writes the column. Allocated on first use with every counter at 1. */
int32_t* flecs_table_get_dirty_state(ecs_world_t *world, ecs_table_t *table);

/* Copies `table`'s change counters into `out` (up to `cap`) and returns how many there are (column count + 1). The
 * first call for a table starts its counting. */
int32_t glue_table_dirty(ecs_world_t *world, ecs_table_t *table, int32_t *out, int32_t cap) {
    if (!table) return 0;
    int32_t n = ecs_table_column_count(table) + 1;
    const int32_t *d = flecs_table_get_dirty_state(world, table);
    for (int32_t i = 0; i < n && i < cap; i++) out[i] = d[i];
    return n;
}

/* An opaque type's serializer, with Rust's callbacks (a context pointer first) in place of ecs_serializer_t's. */
typedef int (*glue_value_t)(void *ctx, ecs_entity_t type, const void *ptr);
typedef int (*glue_member_t)(void *ctx, const char *name);

typedef struct glue_ser_ctx_t {
    glue_value_t value;
    glue_member_t member;
    void *ctx;
} glue_ser_ctx_t;

static int glue_ser_value(const ecs_serializer_t *ser, ecs_entity_t type, const void *ptr) {
    const glue_ser_ctx_t *c = ser->ctx;
    return c->value(c->ctx, type, ptr);
}

static int glue_ser_member(const ecs_serializer_t *ser, const char *name) {
    const glue_ser_ctx_t *c = ser->ctx;
    return c->member ? c->member(c->ctx, name) : -1;
}

/* The type an opaque type serialises as (EcsOpaque.as_type), or 0 when `type` is not opaque. */
ecs_entity_t glue_opaque_as_type(const ecs_world_t *world, ecs_entity_t type) {
    const EcsOpaque *o = ecs_get(world, type, EcsOpaque);
    return o ? o->as_type : 0;
}

/* Runs an opaque type's serialize callback on the value at `ptr`; it calls `value` (and `member`, for a struct-shaped
 * opaque) for each part. Returns -1 when the type has no serialize callback, else what the callback returns. */
int glue_opaque_serialize(const ecs_world_t *world, ecs_entity_t type, const void *ptr, glue_value_t value,
                          glue_member_t member, void *ctx) {
    const EcsOpaque *o = ecs_get(world, type, EcsOpaque);
    if (!o || !o->serialize) return -1;
    glue_ser_ctx_t c = {value, member, ctx};
    ecs_serializer_t ser = {0};
    ser.value = glue_ser_value;
    ser.member = glue_ser_member;
    ser.world = world;
    ser.ctx = &c;
    return o->serialize(&ser, ptr);
}

/* sizeof and alignof of the structs crates/flecs-c/src/lib.rs mirrors, by index. */
size_t glue_layout(int32_t which, int32_t align) {
#define L(T) return align ? _Alignof(T) : sizeof(T)
    switch (which) {
    case 0: L(ecs_type_t);
    case 1: L(EcsType);
    case 2: L(EcsPrimitive);
    case 3: L(ecs_vec_t);
    case 4: L(EcsStruct);
    case 5: L(ecs_member_t);
    case 6: L(EcsEnum);
    case 7: L(ecs_enum_constant_t);
    case 8: L(ecs_bitmask_constant_t);
    case 9: L(EcsConstants);
    case 10: L(EcsArray);
    case 11: L(EcsVector);
    case 12: L(EcsComponent);
    default: return 0;
    }
#undef L
}

/* offsetof of the ecs_member_t fields Rust reads, by index. */
size_t glue_member_offset(int32_t which) {
    switch (which) {
    case 0: return offsetof(ecs_member_t, name);
    case 1: return offsetof(ecs_member_t, type);
    case 2: return offsetof(ecs_member_t, count);
    case 3: return offsetof(ecs_member_t, offset);
    case 4: return offsetof(ecs_member_t, size);
    default: return 0;
    }
}

/* ---- Described types (crates/flecs-describe) ------------------------------------------------------------------------
 * Flecs meta and lifecycle hooks registered from a Rust type's description: the builtin primitive ids by kind, named
 * child types, struct members with explicit offsets (one at a time, so there is no cap of ECS_MEMBER_DESC_CACHE_SIZE),
 * arrays, vectors, enum and bitmask constants with explicit values, opaque types with a serialize callback, components
 * with Rust's hooks, and the serializer calls an opaque callback makes. */

/* The builtin type of a primitive kind (EcsBool ... EcsId), or 0. */
ecs_entity_t glue_describe_primitive(int32_t kind) {
    switch (kind) {
    case EcsBool: return ecs_id(ecs_bool_t);
    case EcsChar: return ecs_id(ecs_char_t);
    case EcsByte: return ecs_id(ecs_byte_t);
    case EcsU8: return ecs_id(ecs_u8_t);
    case EcsU16: return ecs_id(ecs_u16_t);
    case EcsU32: return ecs_id(ecs_u32_t);
    case EcsU64: return ecs_id(ecs_u64_t);
    case EcsI8: return ecs_id(ecs_i8_t);
    case EcsI16: return ecs_id(ecs_i16_t);
    case EcsI32: return ecs_id(ecs_i32_t);
    case EcsI64: return ecs_id(ecs_i64_t);
    case EcsF32: return ecs_id(ecs_f32_t);
    case EcsF64: return ecs_id(ecs_f64_t);
    case EcsUPtr: return ecs_id(ecs_uptr_t);
    case EcsIPtr: return ecs_id(ecs_iptr_t);
    case EcsString: return ecs_id(ecs_string_t);
    case EcsEntity: return ecs_id(ecs_entity_t);
    case EcsId: return ecs_id(ecs_id_t);
    default: return 0;
    }
}

/* The child of `parent` (0: a root) named exactly `name` (no path parsing), made when there is none. A new one is a
 * component of `size` and `align` when size > 0, so Flecs treats it as an existing type of that size. */
ecs_entity_t glue_describe_type(ecs_world_t *world, ecs_entity_t parent, const char *name, int32_t size,
                                int32_t align) {
    ecs_entity_t e = ecs_lookup_child(world, parent, name);
    if (e) return e;
    e = parent ? ecs_new_w_id(world, ecs_pair(EcsChildOf, parent)) : ecs_new(world);
    ecs_set_name(world, e, name);
    if (size > 0) {
        ecs_component_desc_t d = {0};
        d.entity = e;
        d.type.size = size;
        d.type.alignment = align;
        if (!ecs_component_init(world, &d)) return 0;
    }
    return e;
}

/* Adds member `name` of `member_type` (an inline array when count > 1) at Rust's offset. use_offset is set for every
 * member, so Flecs never recomputes C offsets (it would when a member at offset 0 comes without it). */
int glue_describe_member(ecs_world_t *world, ecs_entity_t type, const char *name, ecs_entity_t member_type,
                         int32_t count, int32_t offset) {
    ecs_member_t m = {0};
    m.name = name;
    m.type = member_type;
    m.count = count;
    m.offset = offset;
    m.use_offset = true;
    return ecs_struct_add_member(world, type, &m);
}

ecs_entity_t glue_describe_array(ecs_world_t *world, ecs_entity_t entity, ecs_entity_t elem, int32_t count) {
    ecs_array_desc_t d = {0};
    d.entity = entity;
    d.type = elem;
    d.count = count;
    return ecs_array_init(world, &d);
}

ecs_entity_t glue_describe_vector(ecs_world_t *world, ecs_entity_t entity, ecs_entity_t elem) {
    ecs_vector_desc_t d = {0};
    d.entity = entity;
    d.type = elem;
    return ecs_vector_init(world, &d);
}

/* Makes `entity` an enum over the primitive `underlying`; constants follow with glue_describe_enum_constant. */
int glue_describe_enum(ecs_world_t *world, ecs_entity_t entity, ecs_entity_t underlying) {
    if (!ecs_get(world, underlying, EcsPrimitive)) return -1;
    EcsEnum *e = ecs_ensure(world, entity, EcsEnum);
    if (!e) return -1;
    e->underlying_type = underlying;
    ecs_modified(world, entity, EcsEnum);
    return 0;
}

/* A constant child named exactly `name` whose value is always set explicitly as (Constant, underlying): a bare Constant
 * tag would get Flecs's next value instead of 0. `value` is used for a signed underlying type, `value_unsigned` for an
 * unsigned one, as ecs_enum_init does. */
int glue_describe_enum_constant(ecs_world_t *world, ecs_entity_t type, const char *name, int64_t value,
                                uint64_t value_unsigned) {
    const EcsEnum *en = ecs_get(world, type, EcsEnum);
    if (!en) return -1;
    ecs_entity_t ut = en->underlying_type;
    const EcsPrimitive *p = ecs_get(world, ut, EcsPrimitive);
    const ecs_type_info_t *ti = ecs_get_type_info(world, ut);
    if (!p || !ti) return -1;
    bool is_unsigned = p->kind == EcsU8 || p->kind == EcsU16 || p->kind == EcsU32 || p->kind == EcsU64 ||
                       p->kind == EcsUPtr || p->kind == EcsByte;
    ecs_entity_t c = ecs_new_w_id(world, ecs_pair(EcsChildOf, type));
    ecs_set_name(world, c, name);
    void *ptr = ecs_ensure_id(world, c, ecs_pair(EcsConstant, ut), (size_t)ti->size);
    if (!ptr) return -1;
    ecs_meta_cursor_t cur = ecs_meta_cursor(world, ut, ptr);
    int rc = is_unsigned ? ecs_meta_set_uint(&cur, value_unsigned) : ecs_meta_set_int(&cur, value);
    if (rc) return rc;
    ecs_modified_id(world, c, ecs_pair(EcsConstant, ut));
    return 0;
}

int glue_describe_bitmask(ecs_world_t *world, ecs_entity_t entity) {
    ecs_add(world, entity, EcsBitmask);
    return ecs_has(world, entity, EcsBitmask) ? 0 : -1;
}

int glue_describe_bitmask_constant(ecs_world_t *world, ecs_entity_t type, const char *name, uint32_t value) {
    ecs_entity_t c = ecs_new_w_id(world, ecs_pair(EcsChildOf, type));
    ecs_set_name(world, c, name);
    ecs_set_id(world, c, ecs_pair(EcsConstant, ecs_id(ecs_u32_t)), sizeof(uint32_t), &value);
    return 0;
}

/* Makes `type` (a component) opaque: serialised by `serialize` in the shape of `as_type`. No assign callbacks, so a
 * script or the meta cursor cannot write it. */
ecs_entity_t glue_describe_opaque(ecs_world_t *world, ecs_entity_t type, ecs_entity_t as_type,
                                  ecs_meta_serialize_t serialize) {
    ecs_opaque_desc_t d = {0};
    d.entity = type;
    d.type.as_type = as_type;
    d.type.serialize = serialize;
    return ecs_opaque_init(world, &d);
}

/* A component at path `name` (`.`-separated, made when missing) of `size` and `align`, with every lifecycle hook given
 * explicitly, so Flecs derives none of them. */
ecs_entity_t glue_describe_component(ecs_world_t *world, const char *name, int32_t size, int32_t align,
                                     ecs_xtor_t ctor, ecs_xtor_t dtor, ecs_copy_t copy, ecs_move_t move,
                                     ecs_copy_t copy_ctor, ecs_move_t move_ctor, ecs_move_t ctor_move_dtor,
                                     ecs_move_t move_dtor) {
    ecs_entity_desc_t ed = {0};
    ed.name = name;
    ed.sep = ".";
    ecs_entity_t e = ecs_entity_init(world, &ed);
    if (!e) return 0;
    ecs_component_desc_t d = {0};
    d.entity = e;
    d.type.size = size;
    d.type.alignment = align;
    if (!ecs_component_init(world, &d)) return 0;
    ecs_type_hooks_t h = {0};
    h.ctor = ctor;
    h.dtor = dtor;
    h.copy = copy;
    h.move = move;
    h.copy_ctor = copy_ctor;
    h.move_ctor = move_ctor;
    h.ctor_move_dtor = ctor_move_dtor;
    h.move_dtor = move_dtor;
    ecs_set_hooks_id(world, e, &h);
    return e;
}

int glue_describe_ser_value(const ecs_serializer_t *ser, ecs_entity_t type, const void *ptr) {
    return ser->value(ser, type, ptr);
}

/* -1 when the serializer takes no member names (it is not serialising a struct-shaped opaque). */
int glue_describe_ser_member(const ecs_serializer_t *ser, const char *name) {
    return ser->member ? ser->member(ser, name) : -1;
}

const ecs_world_t *glue_describe_ser_world(const ecs_serializer_t *ser) { return ser->world; }

/* ---- Direct writes (restore) -------------------------------------------------------------------------------------- */

/* A meta cursor over the live value at `ptr` of `type`, on the heap (ecs_meta_cursor_t is returned by value). */
ecs_meta_cursor_t *glue_cursor_new(const ecs_world_t *world, ecs_entity_t type, void *ptr) {
    ecs_meta_cursor_t *c = ecs_os_calloc_t(ecs_meta_cursor_t);
    if (!c) return NULL;
    *c = ecs_meta_cursor(world, type, ptr);
    return c;
}

void glue_cursor_free(ecs_meta_cursor_t *c) { ecs_os_free(c); }

/* Replaces the Flecs string at `slot` (freed) with a copy of `len` bytes at `s`, or NULL when `s` is NULL. */
int glue_set_string(char **slot, const char *s, int32_t len) {
    char *n = NULL;
    if (s) {
        n = ecs_os_malloc(len + 1);
        if (!n) return -1;
        ecs_os_memcpy(n, s, len);
        n[len] = 0;
    }
    ecs_os_free(*slot);
    *slot = n;
    return 0;
}

/* ---- Logical types (crates/flecs-snapshot/src/logical.rs) --------------------------------------------------------- */

/* sync.Range starts unbounded: -DBL_MAX to DBL_MAX (finite, so a snapshot can hold it), so a script sets only the
 * bound it means. */
static void glue_range_ctor(void *ptr, int32_t count, const ecs_type_info_t *ti) {
    (void)ti;
    double *r = ptr;
    for (int32_t i = 0; i < count; i++) {
        r[2 * i] = -DBL_MAX;
        r[2 * i + 1] = DBL_MAX;
    }
}

int glue_range_hooks(ecs_world_t *world, ecs_entity_t range) {
    const ecs_type_info_t *ti = ecs_get_type_info(world, range);
    if (!ti || ti->size != 2 * (ecs_size_t)sizeof(double)) return -1;
    ecs_type_hooks_t h = ti->hooks;
    h.ctor = glue_range_ctor;
    h.flags &= ~((ecs_flags32_t)ECS_TYPE_HOOKS);
    ecs_set_hooks_id(world, range, &h);
    return 0;
}

/* The type of a struct member entity, or 0 when `member` is not one. */
ecs_entity_t glue_member_type(const ecs_world_t *world, ecs_entity_t member) {
    const EcsMember *m = ecs_get(world, member, EcsMember);
    return m ? m->type : 0;
}

/* Mirrors a member's logical rules into Flecs's reflection: its range as EcsMemberRanges (when has_range) and its unit
 * as EcsMember.unit (when `unit` is a Flecs unit). Flecs shows both (REST, the explorer) and enforces neither. */
void glue_member_refine(ecs_world_t *world, ecs_entity_t member, bool has_range, double min, double max,
                        ecs_entity_t unit) {
    if (has_range) {
        EcsMemberRanges *r = ecs_ensure(world, member, EcsMemberRanges);
        r->value.min = min;
        r->value.max = max;
        ecs_modified(world, member, EcsMemberRanges);
    }
    if (unit && ecs_has(world, unit, EcsUnit)) {
        EcsMember *m = ecs_ensure(world, member, EcsMember);
        if (m->unit != unit) {
            m->unit = unit;
            ecs_modified(world, member, EcsMember);
        }
    }
}

#ifdef GLUE_REST
/* A read-only REST server over `world` (Flecs's REST addon, EcsRest's readonly): writes (PUT, DELETE, POST) and
 * function calls reply 403. It is never started, so it opens no socket and starts no thread: the caller owns the
 * listener and hands each request to glue_rest_request on the world's thread. No reply cache. NULL on failure. */
ecs_http_server_t *glue_rest_new(ecs_world_t *world) {
    ecs_http_server_t *srv = ecs_rest_server_init(world, &(ecs_http_server_desc_t){ .cache_timeout = 0 });
    if (!srv) return NULL;
    ecs_rest_ctx_t *ctx = ecs_http_server_ctx(srv);
    ctx->readonly = true;
    return srv;
}

void glue_rest_free(ecs_http_server_t *srv) { ecs_rest_server_fini(srv); }

/* Serves one request: `method` ("GET", ...), `path` the raw request target (percent-encoded path and query), `body`
 * NULL or a NUL-terminated body. Returns the HTTP status code and writes the reply body (ecs_os_malloc'd, NULL when
 * empty; free it with glue_free) and its length, and the content type (a static string). */
int32_t glue_rest_request(ecs_http_server_t *srv, const char *method, const char *path, const char *body,
                          char **body_out, int32_t *len_out, const char **content_type_out) {
    ecs_http_reply_t reply = ECS_HTTP_REPLY_INIT;
    ecs_http_server_request(srv, method, path, body, &reply);
    *len_out = ecs_strbuf_written(&reply.body);
    *body_out = ecs_strbuf_get(&reply.body);
    *content_type_out = reply.content_type;
    ecs_strbuf_reset(&reply.headers);
    return reply.code;
}

void glue_free(void *ptr) { ecs_os_free(ptr); }
#endif
