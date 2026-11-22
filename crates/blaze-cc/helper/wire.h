// blaze-cfront's side of the messages (D174; crates/blaze-cc/src/wire.rs is the table and its
// tests hold the byte samples this file must equal). Version 2.
//
// Every message is a 4-byte big-endian body length, then the body. Integers are big-endian.
//
// From the helper:
//   0x01 Hello       u16 version, u8 confinement (0 none, 1 seatbelt, 2 seccomp, 3 seccomp+landlock)
//   0x02 Look        u32 id, u8 op (1 stat, 2 read, 3 list), path bytes (the rest)
//   0x03 Out         u8 kind (1 object, 2 decls), bytes (the rest; at most 1 MiB a message)
//   0x04 Done        u8 errors (0 or 1)
//   0x05 Diagnostic  u8 level (1 note, 2 remark, 3 warning, 4 error, 5 fatal), u32 code,
//                    u32 offset, u16 path length, path, message (the rest)
// From blaze:
//   0x81 Job         u8 mode (1 decls, 2 compile), u16 count, then count times u32 length + bytes
//   0x82 Answer      u32 id, u8 kind:
//                      0 missing
//                      1 file: u64 size, then for a read the bytes
//                      2 directory
//                      3 listing: u32 count, then count times u16 length + name (a directory's
//                        with a trailing '/'), sorted bytewise
//                      4 not a file: taken as a directory (the import's answer to a stat)
//
// Nothing here decides: blaze's machine (job.rs) checks every message.

#ifndef BLAZE_CFRONT_WIRE_H
#define BLAZE_CFRONT_WIRE_H

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

namespace blaze_wire {

inline constexpr uint16_t VERSION = 2;
inline constexpr size_t PREFIX = 4;
inline constexpr size_t MOST_OUT_PIECE = size_t(1) << 20;

enum class Confinement : uint8_t { None = 0, Seatbelt = 1, Seccomp = 2, SeccompLandlock = 3 };
enum class LookOp : uint8_t { Stat = 1, Read = 2, List = 3 };
enum class OutKind : uint8_t { Object = 1, Decls = 2 };
enum class Level : uint8_t { Note = 1, Remark = 2, Warning = 3, Error = 4, Fatal = 5 };
enum class Mode : uint8_t { Decls = 1, Compile = 2 };

enum class AnswerKind : uint8_t { Missing = 0, File = 1, Dir = 2, Listing = 3, NotAFile = 4 };

inline void put_u8(std::vector<uint8_t> &out, uint8_t v) { out.push_back(v); }
inline void put_u16(std::vector<uint8_t> &out, uint16_t v) {
  out.push_back(uint8_t(v >> 8));
  out.push_back(uint8_t(v));
}
inline void put_u32(std::vector<uint8_t> &out, uint32_t v) {
  for (int shift = 24; shift >= 0; shift -= 8) out.push_back(uint8_t(v >> shift));
}
inline void put_bytes(std::vector<uint8_t> &out, std::string_view bytes) {
  out.insert(out.end(), bytes.begin(), bytes.end());
}

inline uint32_t get_u32(const uint8_t *p) {
  return (uint32_t(p[0]) << 24) | (uint32_t(p[1]) << 16) | (uint32_t(p[2]) << 8) | uint32_t(p[3]);
}
inline uint16_t get_u16(const uint8_t *p) { return uint16_t((uint16_t(p[0]) << 8) | uint16_t(p[1])); }
inline uint64_t get_u64(const uint8_t *p) {
  uint64_t v = 0;
  for (int i = 0; i < 8; ++i) v = (v << 8) | uint64_t(p[i]);
  return v;
}

// A whole message: its prefix, then `body`. False when the body is past a u32's count.
inline bool frame(std::vector<uint8_t> &out, const std::vector<uint8_t> &body) {
  if (body.size() > UINT32_MAX) return false;
  put_u32(out, uint32_t(body.size()));
  out.insert(out.end(), body.begin(), body.end());
  return true;
}

inline std::vector<uint8_t> hello(Confinement confinement) {
  std::vector<uint8_t> body;
  put_u8(body, 0x01);
  put_u16(body, VERSION);
  put_u8(body, uint8_t(confinement));
  return body;
}

inline std::vector<uint8_t> look(uint32_t id, LookOp op, std::string_view path) {
  std::vector<uint8_t> body;
  put_u8(body, 0x02);
  put_u32(body, id);
  put_u8(body, uint8_t(op));
  put_bytes(body, path);
  return body;
}

inline std::vector<uint8_t> out_piece(OutKind kind, std::string_view bytes) {
  std::vector<uint8_t> body;
  put_u8(body, 0x03);
  put_u8(body, uint8_t(kind));
  put_bytes(body, bytes);
  return body;
}

inline std::vector<uint8_t> done(bool errors) {
  std::vector<uint8_t> body;
  put_u8(body, 0x04);
  put_u8(body, errors ? 1 : 0);
  return body;
}

// False when the path is past a u16's count (the job refuses no row for it: the helper stops).
inline bool diagnostic(std::vector<uint8_t> &body, Level level, uint32_t code, uint32_t offset, std::string_view path,
                       std::string_view message) {
  if (path.size() > UINT16_MAX) return false;
  put_u8(body, 0x05);
  put_u8(body, uint8_t(level));
  put_u32(body, code);
  put_u32(body, offset);
  put_u16(body, uint16_t(path.size()));
  put_bytes(body, path);
  put_bytes(body, message);
  return true;
}

// The Job message, read. Nothing on any malformed byte.
struct Job {
  Mode mode;
  std::vector<std::string> args;
};

inline std::optional<Job> read_job(const uint8_t *body, size_t len) {
  if (len < 4 || body[0] != 0x81) return std::nullopt;
  Job job;
  switch (body[1]) {
  case 1: job.mode = Mode::Decls; break;
  case 2: job.mode = Mode::Compile; break;
  default: return std::nullopt;
  }
  uint16_t count = get_u16(body + 2);
  size_t at = 4;
  for (uint16_t i = 0; i < count; ++i) {
    if (len - at < 4) return std::nullopt;
    uint32_t n = get_u32(body + at);
    at += 4;
    if (len - at < n) return std::nullopt;
    job.args.emplace_back(reinterpret_cast<const char *>(body + at), n);
    at += n;
  }
  if (at != len) return std::nullopt;
  return job;
}

// An Answer, read.
struct Answer {
  uint32_t id = 0;
  AnswerKind kind = AnswerKind::Missing;
  uint64_t size = 0;
  std::string bytes;              // a read's file
  std::vector<std::string> names; // a listing's
};

inline std::optional<Answer> read_answer(const uint8_t *body, size_t len, bool read) {
  if (len < 6 || body[0] != 0x82) return std::nullopt;
  Answer a;
  a.id = get_u32(body + 1);
  size_t at = 6;
  switch (body[5]) {
  case 0: a.kind = AnswerKind::Missing; break;
  case 2: a.kind = AnswerKind::Dir; break;
  case 4: a.kind = AnswerKind::NotAFile; break;
  case 1: {
    a.kind = AnswerKind::File;
    if (len - at < 8) return std::nullopt;
    a.size = get_u64(body + at);
    at += 8;
    if (read) {
      if (len - at != a.size) return std::nullopt;
      a.bytes.assign(reinterpret_cast<const char *>(body + at), len - at);
      at = len;
    }
    break;
  }
  case 3: {
    a.kind = AnswerKind::Listing;
    if (len - at < 4) return std::nullopt;
    uint32_t count = get_u32(body + at);
    at += 4;
    for (uint32_t i = 0; i < count; ++i) {
      if (len - at < 2) return std::nullopt;
      uint16_t n = get_u16(body + at);
      at += 2;
      if (len - at < n) return std::nullopt;
      a.names.emplace_back(reinterpret_cast<const char *>(body + at), n);
      at += n;
    }
    break;
  }
  default: return std::nullopt;
  }
  if (at != len) return std::nullopt;
  return a;
}

} // namespace blaze_wire

#endif
