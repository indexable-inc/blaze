// flecs-doc: runs Flecs scripts and prints the documents they build as JSON.
//
//   flecs-doc tree.json site,refs schema.flecs a.flecs b.flecs ...
//
// Every file runs in one world, in order. The output is a JSON array of every entity under the named
// roots, written to the first argument (bun's WASI cannot redirect stdout), depth first in script order (containers have OrderedChildren), one object per entity as
// ecs_entity_to_json writes it, so the site's build step can rebuild the tree.
#include "flecs.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static char *read_file(const char *path) {
    FILE *f = fopen(path, "rb");
    if (!f) return NULL;
    fseek(f, 0, SEEK_END);
    long n = ftell(f);
    fseek(f, 0, SEEK_SET);
    char *buf = malloc((size_t)n + 1);
    if (fread(buf, 1, (size_t)n, f) != (size_t)n) { fclose(f); free(buf); return NULL; }
    buf[n] = '\0';
    fclose(f);
    return buf;
}

static bool first = true;
static FILE *out;

static void emit(ecs_world_t *world, ecs_entity_t e) {
    ecs_entity_to_json_desc_t desc = ECS_ENTITY_TO_JSON_INIT;
    desc.serialize_entity_id = true;
    desc.serialize_full_paths = true;
    desc.serialize_values = true;
    desc.serialize_inherited = true;
    char *json = ecs_entity_to_json(world, e, &desc);
    if (!json) { fprintf(stderr, "flecs-doc: cannot serialize %s\n", ecs_get_path(world, e)); exit(1); }
    fprintf(out, "%s%s\n", first ? "" : ",", json);
    first = false;
    ecs_os_free(json);

    ecs_iter_t it = ecs_children(world, e);
    while (ecs_children_next(&it)) {
        for (int i = 0; i < it.count; i++) emit(world, it.entities[i]);
    }
}

int main(int argc, char **argv) {
    if (argc < 4) { fprintf(stderr, "usage: flecs-doc <out.json> <root,root...> <script.flecs>...\n"); return 2; }
    ecs_world_t *world = ecs_init();
    for (int i = 3; i < argc; i++) {
        char *code = read_file(argv[i]);
        if (!code) { fprintf(stderr, "flecs-doc: cannot read %s\n", argv[i]); return 1; }
        if (ecs_script_run(world, argv[i], code, NULL)) {
            fprintf(stderr, "flecs-doc: %s failed\n", argv[i]);
            return 1;
        }
        free(code);
    }
    out = fopen(argv[1], "w");
    if (!out) { fprintf(stderr, "flecs-doc: cannot write %s\n", argv[1]); return 1; }
    fprintf(out, "[\n");
    for (char *name = strtok(argv[2], ","); name; name = strtok(NULL, ",")) {
        ecs_entity_t root = ecs_lookup(world, name);
        if (!root) { fprintf(stderr, "flecs-doc: no root entity %s\n", name); return 1; }
        emit(world, root);
    }
    fprintf(out, "]\n");
    fclose(out);
    return ecs_fini(world);
}
