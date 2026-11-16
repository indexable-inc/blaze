// LaunchServices starts this executable; exec keeps the native UI in the same app process.
// All runtime paths are relative to the bundle, so moving blaze.app keeps it working.
#include <mach-o/dyld.h>
#include <sys/stat.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <limits.h>

int main(int argc, char **argv) {
    (void)argc;
    char exe[PATH_MAX];
    uint32_t size = sizeof(exe);
    if (_NSGetExecutablePath(exe, &size) != 0) {
        fprintf(stderr, "blaze: app executable path exceeds %zu bytes\n", sizeof(exe));
        return 1;
    }
    char *slash = strrchr(exe, '/');
    if (slash == NULL) return 1;
    *slash = '\0';
    char native[PATH_MAX], resources[PATH_MAX], kernel[PATH_MAX], config[PATH_MAX];
    if (snprintf(native, sizeof(native), "%s/harness-native", exe) >= (int)sizeof(native) ||
        snprintf(resources, sizeof(resources), "%s/../Resources", exe) >= (int)sizeof(resources) ||
        snprintf(kernel, sizeof(kernel), "%s/mcp", resources) >= (int)sizeof(kernel) ||
        snprintf(config, sizeof(config), "%s/tool-path", resources) >= (int)sizeof(config)) {
        fprintf(stderr, "blaze: bundle path exceeds PATH_MAX\n");
        return 1;
    }
    FILE *file = fopen(config, "rb");
    if (file == NULL) { perror(config); return 1; }
    char tools[32768];
    size_t len = fread(tools, 1, sizeof(tools) - 1, file);
    if (ferror(file) || !feof(file)) {
        fprintf(stderr, "blaze: cannot read bounded tool PATH from %s\n", config);
        fclose(file);
        return 1;
    }
    fclose(file);
    tools[len] = '\0';
    char path[sizeof(tools) + PATH_MAX + 1];
    if (snprintf(path, sizeof(path), "%s:%s", exe, tools) >= (int)sizeof(path)) return 1;
    if (setenv("PATH", path, 1) || setenv("HARNESS_ROOT", kernel, 1)) {
        perror("blaze: configure bundled runtime");
        return 1;
    }
    argv[0] = native;
    execv(native, argv);
    perror(native);
    return 1;
}
