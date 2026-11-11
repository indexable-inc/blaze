/* blaze-client's C ABI (docs/client.md): a client of blaze's daemon with no process launch.
 *
 * Every function may be called from any thread, one call at a time for one client. No
 * function unwinds into the caller: a panic inside is error code 13. */
#ifndef BLAZE_CLIENT_H
#define BLAZE_CLIENT_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct blaze_client blaze_client;

/* An error: `code` 0 for none. 1 to 7 are the connection's and the request's faults (1 another
 * schema, 2 bytes refused, 3 an unasked response, 4 the daemon hung up, 5 the deadline passed,
 * 6 a read or write failed, 7 the client was closed); 8 the daemon answered with an error;
 * 9 the connect failed; 10 no daemon listens and none was to be started; 11 the daemon could
 * not be started; 12 what was asked is no request; 13 a panic inside the library; 14 a null or
 * non-UTF-8 argument. `message` is NUL-terminated UTF-8 owned by the library, NULL for none;
 * give the error to blaze_error_free once read. */
typedef struct blaze_error {
    uint32_t code;
    char *message;
} blaze_error;

/* One response of a request: `bytes` (`len` of them, not NUL-terminated) are valid only during
 * the call. JSON is one line, the envelope `{"schema":N,"id":N,"body":<Response>}`; CBOR is
 * the envelope's item as the daemon sent it would decode. */
typedef void (*blaze_event_fn)(void *user, const uint8_t *bytes, size_t len);

enum { BLAZE_JSON = 0, BLAZE_CBOR = 1 };

/* Connects to the daemon of `home` (NULL: the environment's BLAZE_HOME, XDG_RUNTIME_DIR, HOME),
 * starting it when nothing listens, within `within_ms`. NULL on an error, which `error` (not
 * NULL, written always) then says. */
blaze_client *blaze_client_connect(const char *home, uint64_t within_ms, blaze_error *error);

/* Sends one request of the API (`format` BLAZE_JSON: a Request as the API's JSON has it;
 * BLAZE_CBOR: its deterministic CBOR), to end within `within_ms`, and calls `callback` (may be
 * NULL) with each response in the same format, on the calling thread, before it returns.
 * 1 when the request ended ok, 0 when it ended not ok, -1 on an error, which `error` says. */
int32_t blaze_client_request(blaze_client *client, uint32_t format, const uint8_t *bytes,
                             size_t len, uint64_t within_ms, blaze_event_fn callback, void *user,
                             blaze_error *error);

/* One call by name: `method` is "build", "check", "test", "why", "status" or "request", and
 * `options` (`len` bytes of JSON) its options (docs/client.md). Each response goes to
 * `callback` (may be NULL) as JSON. The answer as NUL-terminated JSON, to give to
 * blaze_string_free; NULL on an error, which `error` says. */
char *blaze_client_call(blaze_client *client, const char *method, const uint8_t *options,
                        size_t len, blaze_event_fn callback, void *user, blaze_error *error);

/* Hangs up and frees the client. NULL is allowed. The pointer is dead after. */
void blaze_client_close(blaze_client *client);

/* Frees a string blaze_client_call returned. NULL is allowed. */
void blaze_string_free(char *string);

/* Frees an error's message and sets it to no error. NULL is allowed. */
void blaze_error_free(blaze_error *error);

#ifdef __cplusplus
}
#endif

#endif
