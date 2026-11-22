// blaze-cfront: clang's front end and code generator as a served child of blaze (D174; the design
// is docs/v2/plan/design/c.md "The toolchain side"; the messages are wire.h, whose table is
// crates/blaze-cc/src/wire.rs; blaze's side of one job is crates/blaze-cc/src/job.rs).
//
// What it is, and all it is (c.md decision 2: blaze's one piece of non-Rust code):
//   - the file-system adapter: every path clang asks for goes to blaze as a Look and comes
//     back as an Answer; there is no real file system under it, and the process cannot open a
//     path (it confines itself before its first message);
//   - the argument hand-off: blaze's arguments (flags.rs) through clang's driver to one cc1
//     invocation;
//   - the declaration dump: `cfront-decls 2` (crates/blaze-cc/src/decls.rs is the format), or
//     an object file emitted to memory.
// It decides nothing blaze must trust: job.rs checks every message, and the read list it keeps
// is blaze's, not this file's.
//
// Nothing ambient: the environment is empty (blaze spawns it so) and nothing here reads one;
// no clock is read (the fixed `__DATE__`/`__TIME__` come in as -D arguments); no thread is
// started; the only descriptor used is 3, the channel.
//
// Linked (CMakeLists.txt): clangFrontend, clangCodeGen, clangDriver, clangParse, clangSema,
// clangAnalysis, clangAST, clangEdit, clangLex, clangBasic, clangSerialization, clangSupport;
// LLVM's AArch64 and X86 targets, Core, Support, Option, MC, TargetParser, Passes. On macOS,
// libsandbox (sandbox_init).
//
// The clang API this relies on is listed at the end of this file; the LLVM side (llvm/Support/
// VirtualFileSystem.h, VirtualOutputBackend.h, VirtualOutputFile.h) was read from LLVM 23's
// headers. clang's headers were not on the machine this was written on: the clang names are
// unchecked, and the list says which.

#include "wire.h"

#include "clang/AST/ASTConsumer.h"
#include "clang/AST/ASTContext.h"
#include "clang/AST/Attr.h"
#include "clang/AST/Decl.h"
#include "clang/AST/DeclCXX.h"
#include "clang/AST/Expr.h"
#include "clang/AST/RecordLayout.h"
#include "clang/AST/Stmt.h"
#include "clang/AST/Type.h"
#include "clang/Basic/Diagnostic.h"
#include "clang/Basic/DiagnosticOptions.h"
#include "clang/Basic/SourceManager.h"
#include "clang/Basic/TargetInfo.h"
#include "clang/CodeGen/CodeGenAction.h"
#include "clang/Driver/Compilation.h"
#include "clang/Driver/Driver.h"
#include "clang/Driver/Job.h"
#include "clang/Driver/Tool.h"
#include "clang/Frontend/CompilerInstance.h"
#include "clang/Frontend/CompilerInvocation.h"
#include "clang/Frontend/FrontendAction.h"
#include "clang/Frontend/FrontendActions.h"
#include "clang/Lex/Lexer.h"
#include "clang/Lex/MacroInfo.h"
#include "clang/Lex/Preprocessor.h"

#include "llvm/ADT/APFloat.h"
#include "llvm/ADT/APSInt.h"
#include "llvm/ADT/SmallString.h"
#include "llvm/Support/MemoryBuffer.h"
#include "llvm/Support/Path.h"
#include "llvm/Support/TargetSelect.h"
#include "llvm/Support/VirtualFileSystem.h"
#include "llvm/Support/VirtualOutputBackend.h"
#include "llvm/Support/VirtualOutputFile.h"
#include "llvm/Support/raw_ostream.h"

#include <algorithm>
#include <cerrno>
#include <csignal>
#include <tuple>
#include <map>
#include <memory>
#include <unordered_map>
#include <unordered_set>

#include <sys/socket.h>
#include <unistd.h>

#if defined(__linux__)
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <stddef.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#endif

#if defined(__APPLE__)
// rule 45: libsandbox's `sandbox_init` with a profile string (flags 0) is the private-API form
// of Seatbelt: it takes an SBPL profile, which the public `sandbox_init` names
// (kSBXProfileNoNetwork, ...) do not let us state. Declared here: the SDK marks it deprecated
// and its header leaves the string form out. Its symbol is checked by the ladder's rungs, which
// run the helper (the Hello carries Seatbelt only when it returned 0).
extern "C" int sandbox_init(const char *profile, uint64_t flags, char **errorbuf);
extern "C" void sandbox_free_error(char *errorbuf);
#endif

using namespace clang;
namespace vfs = llvm::vfs;

namespace {

// The descriptor blaze gives the channel (bstd's served child: CHILD_FD).
constexpr int CHANNEL = 3;
// The main file of a declarations job (flags.rs IMPORT_MAIN): held here, never asked for.
constexpr llvm::StringLiteral IMPORT_MAIN = "/.blaze/import.c";
constexpr llvm::StringLiteral IMPORT_DIR = "/.blaze";
// The most one message from blaze may carry: an answer holds a whole file (sqlite3.c is 9 MB;
// job.rs's `most_file` is the bound blaze keeps, this one is the helper's own).
constexpr uint32_t MOST_MESSAGE = 1u << 30;

// The helper cannot go on: it exits, and blaze's job reports the exit (rule 20: one job
// fails). There is nobody to print to; the status names the class.
[[noreturn]] void stop(int status) { _exit(status); }

enum Exit : int { WireBroken = 2, ConfineFailed = 3, BadJob = 4, DriverRefused = 5, FrontEnd = 6 };

// ---------------------------------------------------------------------------------------------
// The channel.

void write_all(const uint8_t *p, size_t n) {
  while (n > 0) {
#if defined(__linux__)
    ssize_t w = send(CHANNEL, p, n, MSG_NOSIGNAL);
#else
    ssize_t w = write(CHANNEL, p, n);
#endif
    if (w < 0 && errno == EINTR) continue;
    if (w <= 0) stop(WireBroken);
    p += w;
    n -= size_t(w);
  }
}

void read_all(uint8_t *p, size_t n) {
  while (n > 0) {
    ssize_t r = read(CHANNEL, p, n);
    if (r < 0 && errno == EINTR) continue;
    if (r <= 0) stop(WireBroken);
    p += r;
    n -= size_t(r);
  }
}

void send(const std::vector<uint8_t> &body) {
  std::vector<uint8_t> framed;
  framed.reserve(body.size() + blaze_wire::PREFIX);
  if (!blaze_wire::frame(framed, body)) stop(WireBroken);
  write_all(framed.data(), framed.size());
}

std::vector<uint8_t> receive() {
  uint8_t prefix[blaze_wire::PREFIX];
  read_all(prefix, sizeof prefix);
  uint32_t len = blaze_wire::get_u32(prefix);
  if (len > MOST_MESSAGE) stop(WireBroken);
  std::vector<uint8_t> body(len);
  read_all(body.data(), len);
  return body;
}

void send_out(blaze_wire::OutKind kind, llvm::StringRef bytes) {
  // Sent in pieces: one message never past MOST_OUT_PIECE (job.rs appends them).
  size_t at = 0;
  do {
    size_t n = std::min(bytes.size() - at, blaze_wire::MOST_OUT_PIECE);
    send(blaze_wire::out_piece(kind, std::string_view(bytes.data() + at, n)));
    at += n;
  } while (at < bytes.size());
}

// ---------------------------------------------------------------------------------------------
// Confinement: before the first message, so nothing the helper does after it can open a path.

blaze_wire::Confinement confine() {
#if defined(__APPLE__)
  // Seatbelt: deny everything; what is left is computation and the descriptor already open.
  // `sysctl-read` stays for LLVM's own queries of the host (page size, CPU count); a signal to
  // itself for abort().
  const char *profile = "(version 1)(deny default)(allow sysctl-read)(allow signal (target self))";
  char *error = nullptr;
  if (sandbox_init(profile, 0, &error) != 0) {
    if (error) sandbox_free_error(error);
    stop(ConfineFailed);
  }
  return blaze_wire::Confinement::Seatbelt;
#elif defined(__linux__)
  if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) stop(ConfineFailed);

  // Landlock first (its own calls are not in the filter's list): a ruleset handling every file
  // access right this kernel's ABI knows, with no rule, so no path is granted.
  bool landlock = false;
  {
    struct Attr {
      uint64_t handled_access_fs;
    };
    // LANDLOCK_CREATE_RULESET_VERSION (1 << 0): the ABI version, no ruleset.
    long abi = syscall(444 /* landlock_create_ruleset */, nullptr, 0, 1u);
    if (abi >= 1) {
      // ABI 1 has bits 0..12; 2 adds REFER (13); 3 adds TRUNCATE (14); 5 adds IOCTL_DEV (15).
      uint64_t bits = (1ull << 13) - 1;
      if (abi >= 2) bits |= 1ull << 13;
      if (abi >= 3) bits |= 1ull << 14;
      if (abi >= 5) bits |= 1ull << 15;
      Attr attr{bits};
      long fd = syscall(444, &attr, sizeof attr, 0u);
      if (fd < 0) stop(ConfineFailed);
      if (syscall(446 /* landlock_restrict_self */, int(fd), 0u) != 0) stop(ConfineFailed);
      close(int(fd));
      landlock = true;
    }
  }

  // seccomp: every call that names a path to open, makes a socket or starts a program fails
  // with EPERM; the rest (memory, reading and writing descriptor 3) is allowed.
#if defined(__aarch64__)
  constexpr uint32_t ARCH = AUDIT_ARCH_AARCH64;
#elif defined(__x86_64__)
  constexpr uint32_t ARCH = AUDIT_ARCH_X86_64;
#else
#error "blaze-cfront: no seccomp architecture for this target"
#endif
  const long denied[] = {
#ifdef __NR_open
      __NR_open,
#endif
#ifdef __NR_creat
      __NR_creat,
#endif
      __NR_openat,
#ifdef __NR_openat2
      __NR_openat2,
#endif
      __NR_open_by_handle_at,
      __NR_socket,
      __NR_socketpair,
      __NR_connect,
      __NR_bind,
      __NR_execve,
      __NR_execveat,
#ifdef __NR_fork
      __NR_fork,
#endif
#ifdef __NR_vfork
      __NR_vfork,
#endif
      __NR_ptrace,
  };
  std::vector<sock_filter> prog;
  prog.push_back(BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(seccomp_data, arch)));
  prog.push_back(BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, ARCH, 1, 0));
  prog.push_back(BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS));
  prog.push_back(BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(seccomp_data, nr)));
  for (long nr : denied) {
    prog.push_back(BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, uint32_t(nr), 0, 1));
    prog.push_back(BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | (EPERM & SECCOMP_RET_DATA)));
  }
  prog.push_back(BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));
  sock_fprog fprog{static_cast<unsigned short>(prog.size()), prog.data()};
  if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &fprog, 0, 0) != 0) stop(ConfineFailed);
  return landlock ? blaze_wire::Confinement::SeccompLandlock : blaze_wire::Confinement::Seccomp;
#else
  return blaze_wire::Confinement::None;
#endif
}

// ---------------------------------------------------------------------------------------------
// The file system: clang's view of the guest, answered by blaze.

// `path` with `.` and empty segments dropped and `..` taking the segment before it away (the
// lexical rule blaze-roots resolves by: the trees hold no symbolic links). Absolute in, out.
std::string lexical(llvm::StringRef path) {
  std::vector<llvm::StringRef> kept;
  llvm::SmallVector<llvm::StringRef, 16> parts;
  path.split(parts, '/');
  for (llvm::StringRef part : parts) {
    if (part.empty() || part == ".") continue;
    if (part == "..") {
      if (!kept.empty()) kept.pop_back();
      continue;
    }
    kept.push_back(part);
  }
  std::string out;
  for (llvm::StringRef part : kept) {
    out += '/';
    out += part.str();
  }
  return out.empty() ? std::string("/") : out;
}

// The paths clang's Linux driver reads to name the host's distribution (clang/lib/Driver/
// Distro.cpp: the target is Linux, so it looks). They are the host's, not the guest's, and no
// mount holds them: answered missing here, without a Look, which is the one constant answer
// blaze would give them too (the guest has no /etc). Exactly these paths; any other goes out.
bool host_probe(llvm::StringRef path) {
  static const char *const probes[] = {"/etc/lsb-release", "/etc/os-release", "/usr/lib/os-release",
                                       "/etc/redhat-release", "/etc/SuSE-release", "/etc/debian_version",
                                       "/etc/alpine-release", "/etc/arch-release", "/etc/gentoo-release"};
  for (const char *probe : probes)
    if (path == probe) return true;
  return false;
}

class WireFS final : public llvm::RTTIExtends<WireFS, vfs::FileSystem> {
public:
  static const char ID;

  // The main file's text for the next parse (a declarations job's probes, or nothing).
  void set_main(std::string text) { main_text_ = std::move(text); }

  llvm::ErrorOr<vfs::Status> status(const llvm::Twine &path) override {
    std::string p = lexical(path.str());
    if (p == IMPORT_MAIN) return file_status(p, main_text_.size());
    if (p == IMPORT_DIR || p == "/") return dir_status(p);
    if (host_probe(p)) return std::make_error_code(std::errc::no_such_file_or_directory);
    auto found = stats_.find(p);
    if (found == stats_.end()) {
      blaze_wire::Answer a = ask(blaze_wire::LookOp::Stat, p);
      Stat s;
      switch (a.kind) {
      case blaze_wire::AnswerKind::Missing: s.kind = Stat::Missing; break;
      case blaze_wire::AnswerKind::File: s.kind = Stat::File; s.size = a.size; break;
      case blaze_wire::AnswerKind::Dir:
      case blaze_wire::AnswerKind::NotAFile: s.kind = Stat::Dir; break;
      case blaze_wire::AnswerKind::Listing: stop(WireBroken);
      }
      found = stats_.emplace(p, s).first;
    }
    switch (found->second.kind) {
    case Stat::Missing: return std::make_error_code(std::errc::no_such_file_or_directory);
    case Stat::Dir: return dir_status(p);
    case Stat::File: break;
    }
    return file_status(p, found->second.size);
  }

  llvm::ErrorOr<std::unique_ptr<vfs::File>> openFileForRead(const llvm::Twine &path) override {
    std::string p = lexical(path.str());
    const std::string *bytes = nullptr;
    if (p == IMPORT_MAIN) {
      bytes = &main_text_;
    } else if (host_probe(p)) {
      return std::make_error_code(std::errc::no_such_file_or_directory);
    } else {
      auto found = files_.find(p);
      if (found == files_.end()) {
        blaze_wire::Answer a = ask(blaze_wire::LookOp::Read, p);
        switch (a.kind) {
        case blaze_wire::AnswerKind::Missing:
          stats_[p] = Stat{Stat::Missing, 0};
          return std::make_error_code(std::errc::no_such_file_or_directory);
        case blaze_wire::AnswerKind::Dir:
        case blaze_wire::AnswerKind::NotAFile:
          stats_[p] = Stat{Stat::Dir, 0};
          return std::make_error_code(std::errc::is_a_directory);
        case blaze_wire::AnswerKind::File: break;
        case blaze_wire::AnswerKind::Listing: stop(WireBroken);
        }
        stats_[p] = Stat{Stat::File, a.size};
        found = files_.emplace(p, std::make_unique<std::string>(std::move(a.bytes))).first;
      }
      bytes = found->second.get();
    }
    return std::unique_ptr<vfs::File>(new Opened(file_status(p, bytes->size()), *bytes));
  }

  vfs::directory_iterator dir_begin(const llvm::Twine &dir, std::error_code &ec) override {
    std::string p = lexical(dir.str());
    auto found = listings_.find(p);
    if (found == listings_.end()) {
      blaze_wire::Answer a = ask(blaze_wire::LookOp::List, p);
      if (a.kind == blaze_wire::AnswerKind::Missing) {
        ec = std::make_error_code(std::errc::no_such_file_or_directory);
        return vfs::directory_iterator();
      }
      if (a.kind != blaze_wire::AnswerKind::Listing) stop(WireBroken);
      found = listings_.emplace(p, std::move(a.names)).first;
    }
    ec = std::error_code();
    return vfs::directory_iterator(std::make_shared<Listing>(p, found->second));
  }

  // The guest has one working directory, `/` (flags.rs gives every path absolute).
  std::error_code setCurrentWorkingDirectory(const llvm::Twine &path) override {
    return lexical(path.str()) == "/" ? std::error_code() : std::make_error_code(std::errc::operation_not_permitted);
  }
  llvm::ErrorOr<std::string> getCurrentWorkingDirectory() const override { return std::string("/"); }

  // Lexical, as blaze resolves: there are no symbolic links in the guest.
  std::error_code getRealPath(const llvm::Twine &path, llvm::SmallVectorImpl<char> &output) override {
    std::string p = lexical(path.str());
    output.assign(p.begin(), p.end());
    return std::error_code();
  }

  std::error_code isLocal(const llvm::Twine &, bool &result) override {
    result = false;
    return std::error_code();
  }

private:
  struct Stat {
    enum Kind { Missing, File, Dir } kind = Missing;
    uint64_t size = 0;
  };

  class Opened final : public vfs::File {
  public:
    Opened(vfs::Status status, const std::string &bytes) : status_(std::move(status)), bytes_(bytes) {}
    llvm::ErrorOr<vfs::Status> status() override { return status_; }
    llvm::ErrorOr<std::unique_ptr<llvm::MemoryBuffer>> getBuffer(const llvm::Twine &name, int64_t, bool nul,
                                                                 bool) override {
      // std::string keeps a NUL past its end, so the buffer may promise one; the bytes live
      // as long as the file system (the caches are never emptied within a job).
      return llvm::MemoryBuffer::getMemBuffer(llvm::StringRef(bytes_.data(), bytes_.size()), name.str(), nul);
    }
    std::error_code close() override { return std::error_code(); }

  private:
    vfs::Status status_;
    const std::string &bytes_;
  };

  class Listing final : public vfs::detail::DirIterImpl {
  public:
    Listing(std::string dir, const std::vector<std::string> &names) : dir_(std::move(dir)), names_(names) { set(); }
    std::error_code increment() override {
      ++at_;
      set();
      return std::error_code();
    }

  private:
    void set() {
      if (at_ >= names_.size()) {
        CurrentEntry = vfs::directory_entry();
        return;
      }
      llvm::StringRef name = names_[at_];
      bool dir = name.consume_back("/");
      std::string path = dir_ == "/" ? "/" + name.str() : dir_ + "/" + name.str();
      CurrentEntry = vfs::directory_entry(
          std::move(path), dir ? llvm::sys::fs::file_type::directory_file : llvm::sys::fs::file_type::regular_file);
    }
    std::string dir_;
    const std::vector<std::string> &names_;
    size_t at_ = 0;
  };

  // A status names no time and no owner (nothing of the machine reaches clang); its unique id
  // is one per path within the job, so clang's file manager sees one file per path.
  vfs::Status file_status(const std::string &p, uint64_t size) {
    return vfs::Status(p, unique(p), llvm::sys::TimePoint<>(), 0, 0, size, llvm::sys::fs::file_type::regular_file,
                       llvm::sys::fs::perms::all_read);
  }
  vfs::Status dir_status(const std::string &p) {
    return vfs::Status(p, unique(p), llvm::sys::TimePoint<>(), 0, 0, 0, llvm::sys::fs::file_type::directory_file,
                       llvm::sys::fs::perms::all_read | llvm::sys::fs::perms::all_exe);
  }
  llvm::sys::fs::UniqueID unique(const std::string &p) {
    auto found = ids_.find(p);
    if (found == ids_.end()) found = ids_.emplace(p, vfs::getNextVirtualUniqueID()).first;
    return found->second;
  }

  blaze_wire::Answer ask(blaze_wire::LookOp op, const std::string &p) {
    uint32_t id = next_id_++;
    send(blaze_wire::look(id, op, p));
    std::vector<uint8_t> body = receive();
    std::optional<blaze_wire::Answer> a = blaze_wire::read_answer(body.data(), body.size(), op == blaze_wire::LookOp::Read);
    if (!a || a->id != id) stop(WireBroken);
    return std::move(*a);
  }

  std::string main_text_;
  uint32_t next_id_ = 0;
  std::map<std::string, Stat> stats_;
  std::map<std::string, std::unique_ptr<std::string>> files_;
  std::map<std::string, std::vector<std::string>> listings_;
  std::map<std::string, llvm::sys::fs::UniqueID> ids_;
};

const char WireFS::ID = 0;

// ---------------------------------------------------------------------------------------------
// Diagnostics, as rows.

class WireDiagnostics final : public DiagnosticConsumer {
public:
  // While probing (a declarations job's second parse), what is said about the probes in the
  // main file is the helper's own business: not sent, not counted.
  bool errors_only = false;
  unsigned sent_errors = 0;

  void HandleDiagnostic(DiagnosticsEngine::Level level, const Diagnostic &info) override {
    DiagnosticConsumer::HandleDiagnostic(level, info);
    blaze_wire::Level wire;
    switch (level) {
    case DiagnosticsEngine::Ignored: return;
    case DiagnosticsEngine::Note: wire = blaze_wire::Level::Note; break;
    case DiagnosticsEngine::Remark: wire = blaze_wire::Level::Remark; break;
    case DiagnosticsEngine::Warning: wire = blaze_wire::Level::Warning; break;
    case DiagnosticsEngine::Error: wire = blaze_wire::Level::Error; break;
    case DiagnosticsEngine::Fatal: wire = blaze_wire::Level::Fatal; break;
    }
    bool error = level >= DiagnosticsEngine::Error;
    if (errors_only && !error) return;
    std::string path;
    uint32_t offset = 0;
    if (info.hasSourceManager() && info.getLocation().isValid()) {
      SourceManager &sm = info.getSourceManager();
      SourceLocation at = sm.getFileLoc(info.getLocation());
      FileID file = sm.getFileID(at);
      if (OptionalFileEntryRef entry = sm.getFileEntryRefForID(file)) path = lexical(entry->getName());
      offset = sm.getFileOffset(at);
    }
    if (path == IMPORT_MAIN) return; // a probe's, by its expansion's place
    llvm::SmallString<256> message;
    info.FormatDiagnostic(message);
    std::vector<uint8_t> body;
    if (!blaze_wire::diagnostic(body, wire, info.getID(), offset, path, std::string_view(message.data(), message.size())))
      stop(WireBroken);
    send(body);
    if (error) ++sent_errors;
  }
};

// ---------------------------------------------------------------------------------------------
// The declaration dump: `cfront-decls 2` (crates/blaze-cc/src/decls.rs). Every line is one call
// of blaze-c's DeclsBuilder; ids are positional, so a line is written only after every id it
// names.

std::string hex(llvm::StringRef bytes) {
  static const char digits[] = "0123456789abcdef";
  std::string out;
  out.reserve(bytes.size() * 2);
  for (unsigned char b : bytes) {
    out += digits[b >> 4];
    out += digits[b & 15];
  }
  return out;
}

// The model's bytes (crates/blaze-c/src/model.rs), as this format spells them.
namespace cause {
constexpr int FunctionLikeMacro = 0, MacroNotConstant = 1, CppConstruct = 3, Atomic = 4, Complex = 5, Vector = 6,
              BitInt = 7, CallingConvention = 10, WideString = 12, FrontEnd = 14, MacroIsFragment = 15,
              LongDouble = 16, NotAValue = 28, CppNamespace = 30, MacroPastesOrAssigns = 37, FunctionTypedef = 42;
}
namespace kind {
constexpr int Function = 0, Variable = 1, Typedef = 2, Record = 3, Enum = 4, Macro = 6;
}

// A macro as the first parse left it, by place (the second parse has its own file ids).
struct MacroSeen {
  std::string name;
  std::string path;
  uint32_t at = 0, len = 0;
  int cause = -1; // -1: probed; else left out for this cause
};

constexpr llvm::StringLiteral PROBE = "__blaze_probe_";

class Dumper {
public:
  Dumper(ASTContext &ctx, const std::vector<MacroSeen> &macros) : ctx_(ctx), sm_(ctx.getSourceManager()), macros_(macros) {}

  std::string run() {
    const TargetInfo &target = ctx_.getTargetInfo();
    out_ += "cfront-decls 2\n";
    out_ += "abi " + std::to_string(target.getPointerWidth(LangAS::Default) / 8) + " " +
            std::to_string(target.getPointerAlign(LangAS::Default) / 8) + " " +
            (target.isLittleEndian() ? "1" : "0") + " " + (target.isCharSigned() ? "1" : "0") + "\n";
    walk(ctx_.getTranslationUnitDecl());
    macros();
    out_ += unexpressed_;
    out_ += "end\n";
    return std::move(out_);
  }

private:
  // ---- places

  struct Place {
    int header = -1;
    uint32_t at = 0, len = 0;
  };

  int header(const std::string &path) {
    auto found = headers_.find(path);
    if (found != headers_.end()) return found->second;
    int id = int(headers_.size());
    headers_.emplace(path, id);
    out_ += "header " + hex(path) + "\n";
    return id;
  }

  // The declaration's range in its header, by expansion (a declaration a macro wrote is where
  // the macro was used). No header for one clang made itself.
  Place place(SourceRange range) {
    Place p;
    if (range.isInvalid()) return p;
    SourceLocation begin = sm_.getExpansionLoc(range.getBegin());
    SourceLocation end = sm_.getExpansionLoc(range.getEnd());
    end = Lexer::getLocForEndOfToken(end, 0, sm_, ctx_.getLangOpts());
    FileID file = sm_.getFileID(begin);
    OptionalFileEntryRef entry = sm_.getFileEntryRefForID(file);
    if (!entry) return p;
    std::string path = lexical(entry->getName());
    if (path == IMPORT_MAIN) return p;
    p.header = header(path);
    p.at = sm_.getFileOffset(begin);
    uint32_t stop_at = end.isValid() && sm_.getFileID(end) == file ? sm_.getFileOffset(end) : p.at;
    p.len = stop_at > p.at ? stop_at - p.at : 0;
    return p;
  }

  static std::string origin(const Place &p) {
    return std::to_string(p.header) + " " + std::to_string(p.at) + " " + std::to_string(p.len);
  }

  // A member's range relative to its record's start (model.rs Field::at).
  std::pair<uint32_t, uint32_t> relative(SourceRange range, const Place &owner) {
    SourceLocation begin = sm_.getExpansionLoc(range.getBegin());
    SourceLocation end = Lexer::getLocForEndOfToken(sm_.getExpansionLoc(range.getEnd()), 0, sm_, ctx_.getLangOpts());
    if (owner.header < 0 || begin.isInvalid() || end.isInvalid() || sm_.getFileID(begin) != sm_.getFileID(end))
      return {0, 0};
    uint32_t b = sm_.getFileOffset(begin), e = sm_.getFileOffset(end);
    if (b < owner.at || e < b) return {0, 0};
    return {b - owner.at, e - b};
  }

  // A name the model takes (model.rs is_c_identifier): a C++ operator or an anonymous
  // namespace has none, and gets no row.
  static bool identifier(llvm::StringRef name) {
    if (name.empty() || (name[0] >= '0' && name[0] <= '9')) return false;
    for (char c : name)
      if (!(llvm::isAlnum(c) || c == '_' || c == '$')) return false;
    return true;
  }

  void left_out(llvm::StringRef name, int decl_kind, int why, const Place &p) {
    if (p.header < 0 || !identifier(name)) return;
    unexpressed_ += "unexpressed " + name.str() + " - " + std::to_string(decl_kind) + " " + std::to_string(why) + " " +
                    origin(p) + "\n";
  }

  // ---- types

  static unsigned quals_byte(Qualifiers q) {
    return (q.hasConst() ? 1 : 0) | (q.hasVolatile() ? 2 : 0) | (q.hasRestrict() ? 4 : 0);
  }

  uint64_t bytes(QualType t) { return ctx_.getTypeSize(t) / 8; }
  uint64_t align(QualType t) { return ctx_.getTypeAlign(t) / 8; }

  uint32_t line(std::string text) {
    out_ += "ty " + text + "\n";
    return types_++;
  }

  uint32_t unexpressible(int why, QualType t) {
    uint64_t size = 0, al = 0;
    if (!t->isIncompleteType() && !t->isFunctionType() && !t->isDependentType()) {
      size = bytes(t);
      al = align(t);
    }
    return line("unexpressible " + std::to_string(why) + " " + std::to_string(size) + " " + std::to_string(al));
  }

  uint32_t ty(QualType t) {
    void *key = t.getAsOpaquePtr();
    auto found = type_ids_.find(key);
    if (found != type_ids_.end()) return found->second;
    uint32_t id = make_ty(t);
    type_ids_.emplace(key, id);
    return id;
  }

  uint32_t make_ty(QualType t) {
    SplitQualType split = t.split();
    const Type *base = split.Ty;
    Qualifiers q = split.Quals;
    if (q.hasAddressSpace() || q.hasObjCLifetime() || q.hasObjCGCAttr()) return unexpressible(cause::FrontEnd, t);
    std::string qs = std::to_string(quals_byte(q));

    // Sugar that names nothing the model keeps: one step through, the qualifiers kept.
    if (isa<ElaboratedType, ParenType, AttributedType, MacroQualifiedType, TypeOfType, TypeOfExprType,
            DecltypeType, BTFTagAttributedType, CountAttributedType>(base))
      return ty(ctx_.getQualifiedType(base->getLocallyUnqualifiedSingleStepDesugaredType(), q));

    if (auto *td = dyn_cast<TypedefType>(base)) {
      TypedefNameDecl *decl = td->getDecl();
      if (decl == ctx_.getBuiltinVaListDecl()) {
        uint32_t lowered = ty(decl->getUnderlyingType().getCanonicalType());
        return line("valist " + std::to_string(lowered) + " " + qs);
      }
      int id = typedef_id(decl);
      if (id < 0) return ty(ctx_.getQualifiedType(decl->getUnderlyingType(), q));
      return line("typedef " + std::to_string(id) + " " + qs);
    }
    if (auto *bt = dyn_cast<BuiltinType>(base)) return builtin(bt, t, qs);
    if (auto *pt = dyn_cast<PointerType>(base)) {
      uint32_t pointee = ty(pt->getPointeeType());
      return line("ptr " + std::to_string(pointee) + " " + qs);
    }
    if (auto *rt = dyn_cast<ReferenceType>(base)) {
      uint32_t pointee = ty(rt->getPointeeTypeAsWritten());
      return line("ref " + std::to_string(pointee) + " " + (isa<RValueReferenceType>(rt) ? "1" : "0"));
    }
    if (auto *at = dyn_cast<ConstantArrayType>(base)) {
      uint32_t elem = ty(at->getElementType());
      return line("array " + std::to_string(elem) + " " + std::to_string(at->getZExtSize()) + " " + qs);
    }
    if (auto *at = dyn_cast<IncompleteArrayType>(base)) {
      uint32_t elem = ty(at->getElementType());
      return line("array " + std::to_string(elem) + " - " + qs);
    }
    if (auto *ft = dyn_cast<FunctionType>(base)) return function_ty(ft, t);
    if (TagDecl *tag = base->getAsTagDecl()) {
      // One clang made itself (AArch64 Linux's `struct __va_list`, the lowering of
      // `__builtin_va_list`) has no header to be from: the model has no row for it.
      TagDecl *where = tag->getDefinition() ? tag->getDefinition() : tag;
      if (tag->isImplicit() || place(where->getSourceRange()).header < 0) return unexpressible(cause::FrontEnd, t);
      if (auto *rd = dyn_cast<RecordDecl>(tag)) {
        uint32_t id = record_id(rd);
        return line("record " + std::to_string(id) + " " + qs);
      }
      if (auto *ed = dyn_cast<EnumDecl>(tag)) {
        uint32_t id = enum_id(ed);
        return line("enum " + std::to_string(id) + " " + qs);
      }
    }
    if (isa<AtomicType>(base)) return unexpressible(cause::Atomic, t);
    if (isa<ComplexType>(base)) return unexpressible(cause::Complex, t);
    if (isa<VectorType>(base)) return unexpressible(cause::Vector, t);
    if (isa<BitIntType>(base)) return unexpressible(cause::BitInt, t);
    return unexpressible(cause::FrontEnd, t);
  }

  uint32_t builtin(const BuiltinType *bt, QualType t, const std::string &qs) {
    auto sized = [&](QualType u) { return std::to_string(bytes(u)) + " " + std::to_string(align(u)); };
    int int_kind = -1, float_kind = -1;
    switch (bt->getKind()) {
    case BuiltinType::Void: return line("void " + qs);
    case BuiltinType::Bool: return line("bool " + sized(t) + " " + qs);
    case BuiltinType::Char_S:
    case BuiltinType::Char_U: int_kind = 0; break;
    case BuiltinType::SChar: int_kind = 1; break;
    case BuiltinType::UChar: int_kind = 2; break;
    case BuiltinType::Short: int_kind = 3; break;
    case BuiltinType::UShort: int_kind = 4; break;
    case BuiltinType::Int: int_kind = 5; break;
    case BuiltinType::UInt: int_kind = 6; break;
    case BuiltinType::Long: int_kind = 7; break;
    case BuiltinType::ULong: int_kind = 8; break;
    case BuiltinType::LongLong: int_kind = 9; break;
    case BuiltinType::ULongLong: int_kind = 10; break;
    case BuiltinType::Int128: int_kind = 11; break;
    case BuiltinType::UInt128: int_kind = 12; break;
    case BuiltinType::Float: float_kind = 0; break;
    case BuiltinType::Double: float_kind = 1; break;
    case BuiltinType::LongDouble: float_kind = 2; break;
    case BuiltinType::Float16:
    case BuiltinType::Half: float_kind = 3; break;
    case BuiltinType::Float128: float_kind = 4; break;
    case BuiltinType::WChar_S:
    case BuiltinType::WChar_U:
    case BuiltinType::Char8:
    case BuiltinType::Char16:
    case BuiltinType::Char32: return unexpressible(cause::WideString, t);
    default: return unexpressible(cause::FrontEnd, t);
    }
    if (int_kind >= 0)
      return line("int " + std::to_string(int_kind) + " " + sized(t) + " " + (bt->isSignedInteger() ? "1" : "0") + " " +
                  qs);
    return line("float " + std::to_string(float_kind) + " " + sized(t) + " " + qs);
  }

  uint32_t function_ty(const FunctionType *ft, QualType t) {
    int conv;
    switch (ft->getCallConv()) {
    case CC_C: conv = 0; break;
    case CC_Win64: conv = 1; break;
    case CC_X86_64SysV: conv = 2; break;
    case CC_X86StdCall: conv = 3; break;
    case CC_X86FastCall: conv = 4; break;
    case CC_X86ThisCall: conv = 5; break;
    case CC_X86VectorCall: conv = 6; break;
    case CC_AAPCS: conv = 7; break;
    default: return unexpressible(cause::CallingConvention, t);
    }
    uint32_t result = ty(ft->getReturnType());
    std::string params;
    bool variadic = false, prototyped = false;
    if (auto *proto = dyn_cast<FunctionProtoType>(ft)) {
      prototyped = true;
      variadic = proto->isVariadic();
      for (QualType p : proto->param_types()) params += " " + std::to_string(ty(p));
    }
    return line("fn " + std::to_string(result) + " " + (variadic ? "1" : "0") + " " + (prototyped ? "1" : "0") + " " +
                std::to_string(conv) + params);
  }

  // ---- named declarations

  // The typedef's id, its line written at first use; -1 for one the model keeps no row for
  // (clang's own, and a typedef of a function type: model.rs FunctionTypedef), whose uses are
  // its type.
  int typedef_id(TypedefNameDecl *decl) {
    decl = cast<TypedefNameDecl>(decl->getCanonicalDecl());
    auto found = typedef_ids_.find(decl);
    if (found != typedef_ids_.end()) return found->second;
    if (decl->isImplicit() || !decl->getDeclContext()->getRedeclContext()->isTranslationUnit()) {
      typedef_ids_.emplace(decl, -1);
      return -1;
    }
    QualType under = decl->getUnderlyingType();
    if (under->isFunctionType()) {
      left_out(decl->getName(), kind::Typedef, cause::FunctionTypedef, place(decl->getSourceRange()));
      typedef_ids_.emplace(decl, -1);
      return -1;
    }
    Place p = place(decl->getSourceRange());
    if (p.header < 0) {
      typedef_ids_.emplace(decl, -1);
      return -1;
    }
    uint32_t t = ty(under);
    int id = typedefs_++;
    typedef_ids_.emplace(decl, id);
    out_ += "typedef " + decl->getName().str() + " - " + std::to_string(t) + " " + origin(p) + "\n";
    return id;
  }

  uint32_t record_id(RecordDecl *rd) {
    rd = cast<RecordDecl>(rd->getCanonicalDecl());
    auto found = record_ids_.find(rd);
    if (found != record_ids_.end()) return found->second;
    RecordDecl *def = rd->getDefinition();
    Place p = place((def ? def : rd)->getSourceRange());
    uint32_t id = records_++;
    record_ids_.emplace(rd, id);
    std::string name = rd->getIdentifier() ? rd->getName().str() : "-";
    out_ += "record " + std::string(rd->isUnion() ? "1" : "0") + " " + name + " - " + origin(p) + "\n";
    return id;
  }

  uint32_t enum_id(EnumDecl *ed) {
    ed = cast<EnumDecl>(ed->getCanonicalDecl());
    auto found = enum_ids_.find(ed);
    if (found != enum_ids_.end()) return found->second;
    EnumDecl *def = ed->getDefinition();
    Place p = place((def ? def : ed)->getSourceRange());
    uint32_t id = enums_++;
    enum_ids_.emplace(ed, id);
    std::string name = ed->getIdentifier() ? ed->getName().str() : "-";
    out_ += "enum " + name + " - " + origin(p) + "\n";
    return id;
  }

  // A tag the walk may give a row: from a header (record_id and enum_id need its place).
  bool placed(TagDecl *tag) {
    TagDecl *where = tag->getDefinition() ? tag->getDefinition() : tag;
    return !tag->isImplicit() && place(where->getSourceRange()).header >= 0;
  }

  void define_record(RecordDecl *rd) {
    RecordDecl *canon = cast<RecordDecl>(rd->getCanonicalDecl());
    if (!rd->isCompleteDefinition() || rd->isInvalidDecl() || !defined_records_.insert(canon).second) return;
    uint32_t id = record_id(rd);
    Place owner = place(rd->getSourceRange());
    const ASTRecordLayout &layout = ctx_.getASTRecordLayout(rd);

    // Field types first: their lines before the block.
    struct FieldRow {
      std::string name;
      uint32_t type;
      uint64_t offset;
      int width = -1, unit = -1;
      uint32_t at, len;
    };
    std::vector<FieldRow> rows;
    std::vector<std::pair<uint64_t, uint64_t>> units; // (offset, size) in bytes
    unsigned index = 0;
    for (FieldDecl *field : rd->fields()) {
      uint64_t offset = layout.getFieldOffset(index++);
      if (field->isZeroLengthBitField()) continue; // layout only: the offsets already hold it
      FieldRow row;
      row.name = field->getIdentifier() ? field->getName().str() : "-";
      row.type = ty(field->getType());
      row.offset = offset;
      auto [at, len] = relative(field->getSourceRange(), owner);
      row.at = at;
      row.len = len;
      if (field->isBitField()) {
        uint64_t width = field->getBitWidthValue();
        // The storage unit: the declared type's aligned unit that holds the field, as a C
        // reader of the field reads it; a packed field that crosses it gets the bytes it
        // touches. Clamped to the record. (Not CodeGen's access units: UNSURE in c.md.)
        uint64_t unit_bytes = std::max<uint64_t>(1, bytes(field->getType()));
        uint64_t start = (offset / 8 / unit_bytes) * unit_bytes;
        uint64_t end = start + unit_bytes;
        if ((offset + width + 7) / 8 > end || end > uint64_t(layout.getSize().getQuantity())) {
          start = offset / 8;
          end = (offset + width + 7) / 8;
        }
        std::pair<uint64_t, uint64_t> unit{start, end - start};
        if (units.empty() || units.back() != unit) units.push_back(unit);
        row.width = int(width);
        row.unit = int(units.size() - 1);
      }
      rows.push_back(std::move(row));
    }
    std::vector<std::string> bases;
    int relocation = 0; // TriviallyCopyable: every C record
    if (auto *cxx = dyn_cast<CXXRecordDecl>(rd)) {
      relocation = cxx->isTriviallyCopyable() ? 0 : 2;
      for (const CXXBaseSpecifier &base : cxx->bases()) {
        CXXRecordDecl *b = base.getType()->getAsCXXRecordDecl();
        if (!b) continue;
        uint64_t off = base.isVirtual() ? uint64_t(layout.getVBaseClassOffset(b).getQuantity())
                                        : uint64_t(layout.getBaseClassOffset(b).getQuantity());
        bases.push_back(std::to_string(record_id(b)) + " " + std::to_string(off) + " " + (base.isVirtual() ? "1" : "0"));
      }
    }
    std::string pack = "-";
    if (rd->hasAttr<PackedAttr>()) pack = "1";
    else if (auto *max = rd->getAttr<MaxFieldAlignmentAttr>()) pack = std::to_string(max->getAlignment() / 8);

    out_ += "define-record " + std::to_string(id) + " " + std::to_string(layout.getSize().getQuantity()) + " " +
            std::to_string(layout.getAlignment().getQuantity()) + " " + pack + " " + std::to_string(relocation) + "\n";
    for (auto &[offset, size] : units) out_ += "unit " + std::to_string(offset) + " " + std::to_string(size) + "\n";
    for (const FieldRow &row : rows)
      out_ += "field " + row.name + " " + std::to_string(row.type) + " " + std::to_string(row.offset) + " " +
              (row.width < 0 ? "-" : std::to_string(row.width)) + " " +
              (row.unit < 0 ? "-" : std::to_string(row.unit)) + " " + std::to_string(row.at) + " " +
              std::to_string(row.len) + "\n";
    for (const std::string &base : bases) out_ += "base " + base + "\n";
    out_ += "end\n";

    // Tags defined inside it (C gives them file scope): after the block, which is whole.
    for (Decl *inner : rd->decls()) {
      if (auto *nested = dyn_cast<RecordDecl>(inner); nested && placed(nested)) define_record(nested);
      else if (auto *nested_enum = dyn_cast<EnumDecl>(inner); nested_enum && placed(nested_enum)) define_enum(nested_enum);
    }
  }

  void define_enum(EnumDecl *ed) {
    EnumDecl *canon = cast<EnumDecl>(ed->getCanonicalDecl());
    if (ed->isInvalidDecl() || defined_enums_.count(canon)) return;
    bool complete = ed->isCompleteDefinition();
    // A forward declaration without a fixed type has nothing to define yet.
    if (!complete && !ed->isFixed()) return;
    if (!complete && ed->getDefinition()) return; // the definition will come
    defined_enums_.insert(canon);
    uint32_t id = enum_id(ed);
    uint32_t underlying = ty(ed->getIntegerType());
    Place owner = place(ed->getSourceRange());
    out_ += "define-enum " + std::to_string(id) + " " + std::to_string(underlying) + " " + (complete ? "1" : "0") + " " +
            (ed->isScoped() ? "1" : "0") + "\n";
    for (EnumConstantDecl *c : ed->enumerators()) {
      llvm::APSInt v = c->getInitVal();
      uint64_t bits = v.isSigned() ? uint64_t(v.extOrTrunc(64).getSExtValue()) : v.extOrTrunc(64).getZExtValue();
      auto [at, len] = relative(c->getSourceRange(), owner);
      out_ += "const " + c->getName().str() + " " + std::to_string(bits) + " " + std::to_string(at) + " " +
              std::to_string(len) + "\n";
    }
    out_ += "end\n";
  }

  // The symbol and its attribute bit (model.rs FnAttrs/VarAttrs SYMBOL_LITERAL: an asm label).
  std::pair<std::string, unsigned> symbol(NamedDecl *d) {
    if (auto *label = d->getAttr<AsmLabelAttr>()) return {label->getLabel().str(), 16};
    return {d->getName().str(), 0};
  }

  void function(FunctionDecl *fd) {
    FunctionDecl *canon = fd->getCanonicalDecl();
    if (!functions_.insert(canon).second) return;
    FunctionDecl *def = fd->getDefinition();
    FunctionDecl *use = def ? def : fd;
    Place p = place(use->getSourceRange());
    if (p.header < 0) return;
    if (!fd->getIdentifier()) return;
    if (ctx_.getLangOpts().CPlusPlus && !fd->isExternC()) {
      left_out(fd->getName(), kind::Function, cause::CppConstruct, p);
      return;
    }
    uint32_t t = ty(use->getType());
    auto [sym, literal] = symbol(use);
    unsigned attrs = literal;
    if (use->isNoReturn()) attrs |= 1;
    if (use->hasAttr<ReturnsTwiceAttr>()) attrs |= 2;
    if (use->hasAttr<DeprecatedAttr>() || use->hasAttr<UnavailableAttr>()) attrs |= 4;
    if (ctx_.getLangOpts().CPlusPlus) {
      auto *proto = use->getType()->getAs<FunctionProtoType>();
      if (proto && !proto->isNothrow()) attrs |= 8;
    }
    // model.rs Linkage: `static` internal; C99 `inline` without `extern` an inline definition;
    // GNU `extern inline` and the rest external.
    int linkage = 0;
    if (use->getStorageClass() == SC_Static) linkage = 1;
    else if (def && def->isInlined() && def->getStorageClass() != SC_Extern && !def->isInlineDefinitionExternallyVisible())
      linkage = 2;
    std::string names;
    if (use->getType()->getAs<FunctionProtoType>())
      for (ParmVarDecl *param : use->parameters())
        names += " " + (param->getIdentifier() ? param->getName().str() : std::string("-"));
    out_ += "fn " + fd->getName().str() + " - " + sym + " " + std::to_string(t) + " " + std::to_string(attrs) + " - 0 " +
            std::to_string(linkage) + " " + (def ? "1" : "0") + " " + origin(p) + names + "\n";
  }

  // A file-scope variable's id, its line written at first use.
  int var_id(VarDecl *vd) {
    VarDecl *canon = vd->getCanonicalDecl();
    auto found = var_ids_.find(canon);
    if (found != var_ids_.end()) return found->second;
    VarDecl *def = vd->getDefinition();
    VarDecl *acting = def ? def : vd->getActingDefinition();
    VarDecl *use = acting ? acting : vd;
    Place p = place(use->getSourceRange());
    if (p.header < 0 || !vd->getIdentifier() || (ctx_.getLangOpts().CPlusPlus && !vd->isExternC())) {
      var_ids_.emplace(canon, -1);
      if (p.header >= 0 && vd->getIdentifier()) left_out(vd->getName(), kind::Variable, cause::CppConstruct, p);
      return -1;
    }
    uint32_t t = ty(use->getType());
    auto [sym, literal] = symbol(use);
    unsigned attrs = literal | (use->getTLSKind() != VarDecl::TLS_None ? 1 : 0);
    int linkage = use->getStorageClass() == SC_Static ? 1 : 0;
    std::string init = "absent";
    if (def && def->hasInit()) {
      init = "other";
      const Expr *e = def->getInit();
      Expr::EvalResult r;
      if (e->isNullPointerConstant(ctx_, Expr::NPC_ValueDependentIsNotNull) != Expr::NPCK_NotNull) {
        init = "zero";
      } else if (e->getType()->isIntegralOrEnumerationType() && e->EvaluateAsInt(r, ctx_)) {
        llvm::APSInt v = r.Val.getInt();
        if (v.getSignificantBits() <= 64 || (v.isUnsigned() && v.getActiveBits() <= 64))
          init = v.isZero() ? "zero" : std::to_string(v.isSigned() ? uint64_t(v.getSExtValue()) : v.getZExtValue());
      }
    } else if (acting) {
      init = "zero"; // a tentative definition: C zero-initializes it
    }
    int id = vars_++;
    var_ids_.emplace(canon, id);
    out_ += "var " + vd->getName().str() + " - " + sym + " " + std::to_string(t) + " " + std::to_string(attrs) + " " +
            std::to_string(linkage) + " " + init + " " + origin(p) + "\n";
    return id;
  }

  // ---- the walk

  void walk(DeclContext *dc) {
    for (Decl *d : dc->decls()) {
      if (d->isImplicit() || d->isInvalidDecl()) continue;
      if (auto *spec = dyn_cast<LinkageSpecDecl>(d)) {
        walk(spec);
        continue;
      }
      if (auto *ns = dyn_cast<NamespaceDecl>(d)) {
        // C++ namespaces: their declarations are X3's work (c.md), not this dump's yet.
        if (ns->getIdentifier()) left_out(ns->getName(), kind::Record, cause::CppNamespace, place(ns->getSourceRange()));
        continue;
      }
      if (auto *fd = dyn_cast<FunctionDecl>(d)) {
        if (fd->getIdentifier() && fd->getName().starts_with(PROBE)) probes_.push_back(fd);
        else if (fd->getBuiltinID() == 0) function(fd);
        continue;
      }
      if (auto *td = dyn_cast<TypedefNameDecl>(d)) {
        typedef_id(td);
        continue;
      }
      if (auto *rd = dyn_cast<RecordDecl>(d)) {
        if (!placed(rd)) continue;
        record_id(rd);
        define_record(rd);
        continue;
      }
      if (auto *ed = dyn_cast<EnumDecl>(d)) {
        if (!placed(ed)) continue;
        enum_id(ed);
        define_enum(ed);
        continue;
      }
      if (auto *vd = dyn_cast<VarDecl>(d)) {
        if (vd->isFileVarDecl()) var_id(vd);
        continue;
      }
    }
  }

  // ---- macros: what the probes (one per object-like macro, in the main file after every
  // header) say each one is, by clang's own reading of the expansion.

  void macros() {
    std::unordered_map<std::string, const FunctionDecl *> by_name;
    for (const FunctionDecl *fd : probes_) by_name.emplace(fd->getName().str(), fd);
    size_t probe = 0;
    for (const MacroSeen &m : macros_) {
      Place p;
      p.header = header(m.path);
      p.at = m.at;
      p.len = m.len;
      if (m.cause >= 0) {
        left_out(m.name, kind::Macro, m.cause, p);
        continue;
      }
      auto found = by_name.find(PROBE.str() + std::to_string(probe++));
      const Expr *e = found == by_name.end() ? nullptr : probed(found->second);
      if (!e) {
        left_out(m.name, kind::Macro, cause::MacroIsFragment, p);
        continue;
      }
      std::string row = constant(m.name, e, p);
      if (row.empty()) left_out(m.name, kind::Macro, last_cause_, p);
      else out_ += row;
    }
  }

  // The probe's `(void)(NAME)`: the expression inside, or none when clang refused it.
  static const Expr *probed(const FunctionDecl *fd) {
    if (fd->isInvalidDecl()) return nullptr;
    auto *body = dyn_cast_or_null<CompoundStmt>(fd->getBody());
    if (!body || body->size() != 1) return nullptr;
    auto *cast = dyn_cast<CStyleCastExpr>(*body->body_begin());
    if (!cast || cast->containsErrors()) return nullptr;
    return cast->getSubExpr()->IgnoreParens();
  }

  int last_cause_ = cause::MacroNotConstant;

  // The macro's row, or empty with `last_cause_` set.
  std::string constant(const std::string &name, const Expr *e, const Place &p) {
    const Expr *bare = e->IgnoreParenImpCasts();
    if (auto *s = dyn_cast<StringLiteral>(bare)) {
      if (!s->isOrdinary()) {
        last_cause_ = cause::WideString;
        return "";
      }
      QualType ch = ctx_.getAsArrayType(s->getType())->getElementType();
      std::string bytes = s->getBytes().empty() ? "-" : hex(s->getBytes());
      return "macro str " + name + " " + std::to_string(ty(ch)) + " " + bytes + " " + origin(p) + "\n";
    }
    if (auto *u = dyn_cast<UnaryOperator>(bare); u && u->getOpcode() == UO_AddrOf) {
      if (auto *ref = dyn_cast<DeclRefExpr>(u->getSubExpr()->IgnoreParens()))
        if (auto *vd = dyn_cast<VarDecl>(ref->getDecl()); vd && vd->isFileVarDecl()) {
          int id = var_id(vd);
          if (id >= 0) return "macro addr " + name + " " + std::to_string(id) + " " + origin(p) + "\n";
        }
    }
    if (std::string slot = table_slot(name, e, p); !slot.empty()) return slot;
    QualType t = e->getType();
    if (t->isIntegralOrEnumerationType()) {
      Expr::EvalResult r;
      if (e->EvaluateAsInt(r, ctx_)) {
        llvm::APSInt v = r.Val.getInt();
        if (v.getBitWidth() > 64 && !(v.isSigned() ? v.isSignedIntN(64) : v.isIntN(64))) {
          last_cause_ = cause::FrontEnd;
          return "";
        }
        uint64_t bits = v.isSigned() ? uint64_t(v.extOrTrunc(64).getSExtValue()) : v.extOrTrunc(64).getZExtValue();
        return "macro int " + name + " " + std::to_string(ty(t)) + " " + std::to_string(bits) + " " + origin(p) + "\n";
      }
    }
    if (t->isRealFloatingType()) {
      const BuiltinType *bt = t->getAs<BuiltinType>();
      if (bt && bt->getKind() == BuiltinType::LongDouble) {
        last_cause_ = cause::LongDouble;
        return "";
      }
      llvm::APFloat f(0.0);
      if (bt && (bt->getKind() == BuiltinType::Float || bt->getKind() == BuiltinType::Double) &&
          e->EvaluateAsFloat(f, ctx_)) {
        bool lost = false;
        f.convert(llvm::APFloat::IEEEdouble(), llvm::APFloat::rmNearestTiesToEven, &lost);
        uint64_t bits = f.bitcastToAPInt().getZExtValue();
        return "macro float " + name + " " + std::to_string(ty(t)) + " " + std::to_string(bits) + " " + origin(p) + "\n";
      }
    }
    // An expression clang read but no constant the model holds: a call, a pointer cast of a
    // number (`((void *)0)`), a variable's value.
    last_cause_ = cause::MacroNotConstant;
    return "";
  }

  // `(*(T *)TABLE[i])` or `((T *)TABLE[i])` over a file-scope pointer or array variable
  // (numpy __multiarray_api.h:652, :650): model.rs MacroValue::Slot.
  std::string table_slot(const std::string &name, const Expr *e, const Place &p) {
    bool deref = false;
    const Expr *at = e->IgnoreParens();
    if (auto *u = dyn_cast<UnaryOperator>(at); u && u->getOpcode() == UO_Deref) {
      deref = true;
      at = u->getSubExpr()->IgnoreParens();
    }
    auto *cast = dyn_cast<CStyleCastExpr>(at);
    if (!cast || !cast->getType()->isPointerType()) return "";
    auto *sub = dyn_cast<ArraySubscriptExpr>(cast->getSubExpr()->IgnoreParenImpCasts());
    if (!sub) return "";
    auto *ref = dyn_cast<DeclRefExpr>(sub->getBase()->IgnoreParenImpCasts());
    auto *vd = ref ? dyn_cast<VarDecl>(ref->getDecl()) : nullptr;
    if (!vd || !vd->isFileVarDecl()) return "";
    QualType table = vd->getType();
    if (!table->isPointerType() && !table->isArrayType()) return "";
    Expr::EvalResult r;
    if (!sub->getIdx()->EvaluateAsInt(r, ctx_) || r.Val.getInt().isNegative()) return "";
    int id = var_id(vd);
    if (id < 0) return "";
    uint32_t t = ty(cast->getType());
    return "macro slot " + name + " " + std::to_string(t) + " " + std::to_string(id) + " " +
           std::to_string(r.Val.getInt().getZExtValue()) + " " + (deref ? "1" : "0") + " " + origin(p) + "\n";
  }

  ASTContext &ctx_;
  SourceManager &sm_;
  const std::vector<MacroSeen> &macros_;
  std::string out_;
  std::string unexpressed_;
  std::map<std::string, int> headers_;
  std::unordered_map<void *, uint32_t> type_ids_;
  std::unordered_map<const TypedefNameDecl *, int> typedef_ids_;
  std::unordered_map<const RecordDecl *, uint32_t> record_ids_;
  std::unordered_map<const EnumDecl *, uint32_t> enum_ids_;
  std::unordered_map<const VarDecl *, int> var_ids_;
  std::unordered_set<const RecordDecl *> defined_records_;
  std::unordered_set<const EnumDecl *> defined_enums_;
  std::unordered_set<const FunctionDecl *> functions_;
  std::vector<const FunctionDecl *> probes_;
  uint32_t types_ = 0, records_ = 0, enums_ = 0, vars_ = 0;
  int typedefs_ = 0;
};

// ---------------------------------------------------------------------------------------------
// The first parse of a declarations job: the preprocessor alone, to the main file's end; then
// every macro the headers left defined, sorted into what a probe reads and what is left out.

class MacroScan final : public PreprocessorFrontendAction {
public:
  explicit MacroScan(std::vector<MacroSeen> &out) : out_(out) {}

protected:
  void ExecuteAction() override {
    Preprocessor &pp = getCompilerInstance().getPreprocessor();
    SourceManager &sm = pp.getSourceManager();
    pp.EnterMainSourceFile();
    Token token;
    do pp.Lex(token);
    while (token.isNot(tok::eof));
    for (const auto &entry : pp.macros()) {
      const IdentifierInfo *id = entry.first;
      const MacroInfo *mi = pp.getMacroInfo(id);
      if (!mi || mi->isBuiltinMacro() || mi->isUsedForHeaderGuard()) continue;
      SourceLocation begin = mi->getDefinitionLoc();
      if (begin.isInvalid() || sm.isWrittenInBuiltinFile(begin) || sm.isWrittenInCommandLineFile(begin)) continue;
      OptionalFileEntryRef entry_ref = sm.getFileEntryRefForID(sm.getFileID(begin));
      if (!entry_ref) continue;
      MacroSeen m;
      m.name = id->getName().str();
      m.path = lexical(entry_ref->getName());
      if (m.path == IMPORT_MAIN) continue;
      SourceLocation end =
          Lexer::getLocForEndOfToken(mi->getDefinitionEndLoc(), 0, sm, getCompilerInstance().getLangOpts());
      m.at = sm.getFileOffset(begin);
      m.len = end.isValid() && sm.getFileOffset(end) > m.at ? sm.getFileOffset(end) - m.at : 0;
      bool pastes = false;
      for (const Token &t : mi->tokens())
        if (t.is(tok::hashhash) || t.is(tok::hash) || t.is(tok::equal) || t.is(tok::plusequal) ||
            t.is(tok::minusequal) || t.is(tok::plusplus) || t.is(tok::minusminus))
          pastes = true;
      if (mi->isFunctionLike()) m.cause = pastes ? cause::MacroPastesOrAssigns : cause::FunctionLikeMacro;
      else if (mi->getNumTokens() == 0) m.cause = cause::NotAValue;
      else if (pastes) m.cause = cause::MacroPastesOrAssigns;
      out_.push_back(std::move(m));
    }
    // pp.macros() is a hash table: the rows are put in the headers' order, so two runs write
    // equal bytes (rule 25).
    std::sort(out_.begin(), out_.end(), [](const MacroSeen &a, const MacroSeen &b) {
      return std::tie(a.path, a.at, a.name) < std::tie(b.path, b.at, b.name);
    });
  }

private:
  std::vector<MacroSeen> &out_;
};

std::string probes(const std::vector<MacroSeen> &macros) {
  std::string text;
  size_t n = 0;
  for (const MacroSeen &m : macros) {
    if (m.cause >= 0) continue;
    text += "static void " + PROBE.str() + std::to_string(n++) + "(void) { (void)(" + m.name + "); }\n";
  }
  return text;
}

class DumpConsumer final : public ASTConsumer {
public:
  DumpConsumer(const std::vector<MacroSeen> &macros, std::string &out) : macros_(macros), out_(out) {}
  void HandleTranslationUnit(ASTContext &ctx) override { out_ = Dumper(ctx, macros_).run(); }

private:
  const std::vector<MacroSeen> &macros_;
  std::string &out_;
};

class DumpAction final : public ASTFrontendAction {
public:
  DumpAction(const std::vector<MacroSeen> &macros, std::string &out) : macros_(macros), out_(out) {}

protected:
  std::unique_ptr<ASTConsumer> CreateASTConsumer(CompilerInstance &, llvm::StringRef) override {
    return std::make_unique<DumpConsumer>(macros_, out_);
  }

private:
  const std::vector<MacroSeen> &macros_;
  std::string &out_;
};

// ---------------------------------------------------------------------------------------------
// A compile job's outputs, in memory: clang's object file goes to `object`, any other output
// clang would make is refused (the arguments ask for none).

class MemoryBackend final : public vfs::OutputBackend {
public:
  explicit MemoryBackend(std::string &object) : object_(object) {}

protected:
  llvm::IntrusiveRefCntPtr<vfs::OutputBackend> cloneImpl() const override {
    return llvm::makeIntrusiveRefCnt<MemoryBackend>(object_);
  }
  llvm::Expected<std::unique_ptr<vfs::OutputFileImpl>> createFileImpl(llvm::StringRef path,
                                                                        std::optional<vfs::OutputConfig>) override {
    if (llvm::sys::path::extension(path) != ".o")
      return llvm::createStringError(std::make_error_code(std::errc::permission_denied),
                                     "blaze-cfront: an output other than the object: " + path);
    return std::make_unique<Kept>(object_);
  }

private:
  class Kept final : public llvm::RTTIExtends<Kept, vfs::OutputFileImpl> {
  public:
    static char ID;
    explicit Kept(std::string &into) : into_(into), os_(bytes_) {}
    llvm::Error keep() override {
      into_.assign(bytes_.data(), bytes_.size());
      return llvm::Error::success();
    }
    llvm::Error discard() override { return llvm::Error::success(); }
    llvm::raw_pwrite_stream &getOS() override { return os_; }

  private:
    std::string &into_;
    llvm::SmallVector<char, 0> bytes_;
    llvm::raw_svector_ostream os_;
  };
  std::string &object_;
};

char MemoryBackend::Kept::ID = 0;

// ---------------------------------------------------------------------------------------------
// One job.

// blaze's arguments (flags.rs: argv[0] is the shelf's clang) through clang's driver to the one
// cc1 command they make, as an invocation. The driver's own diagnostics are rows like any.
std::shared_ptr<CompilerInvocation> invocation(const std::vector<std::string> &args, DiagnosticsEngine &diags,
                                               llvm::IntrusiveRefCntPtr<vfs::FileSystem> fs) {
  if (args.empty()) stop(BadJob);
  std::vector<const char *> argv;
  for (const std::string &a : args) argv.push_back(a.c_str());
  std::string triple;
  for (const std::string &a : args)
    if (llvm::StringRef(a).starts_with("--target=")) triple = a.substr(9);
  if (triple.empty()) stop(BadJob);
  driver::Driver driver(args[0], triple, diags, "blaze-cfront", fs);
  driver.setCheckInputsExist(true);
  std::unique_ptr<driver::Compilation> compilation(driver.BuildCompilation(argv));
  if (!compilation || compilation->containsError() || diags.hasErrorOccurred()) return nullptr;
  const driver::JobList &jobs = compilation->getJobs();
  if (jobs.size() != 1) stop(DriverRefused);
  const driver::Command &command = *jobs.begin();
  if (llvm::StringRef(command.getCreator().getName()) != "clang") stop(DriverRefused);
  const llvm::opt::ArgStringList &cc1 = command.getArguments();
  auto inv = std::make_shared<CompilerInvocation>();
  if (!CompilerInvocation::CreateFromArgs(*inv, cc1, diags, argv[0])) return nullptr;
  return inv;
}

// One parse over `fs`, its diagnostics to `diags`. False when clang said it failed.
bool parse(std::shared_ptr<CompilerInvocation> inv, llvm::IntrusiveRefCntPtr<vfs::FileSystem> fs,
           WireDiagnostics &diags, FrontendAction &action, llvm::IntrusiveRefCntPtr<vfs::OutputBackend> outputs) {
  CompilerInstance ci(std::make_shared<CompilerInvocation>(*inv));
  ci.setVirtualFileSystem(fs);
  ci.createDiagnostics(&diags, /*ShouldOwnClient=*/false);
  ci.createFileManager();
  if (outputs) ci.setOutputBackend(std::move(outputs));
  return ci.ExecuteAction(action);
}

void run(const blaze_wire::Job &job) {
  llvm::IntrusiveRefCntPtr<WireFS> fs = llvm::makeIntrusiveRefCnt<WireFS>();
  WireDiagnostics consumer;
  DiagnosticOptions diag_opts;
  DiagnosticsEngine diags(DiagnosticIDs::create(), diag_opts, &consumer, /*ShouldOwnClient=*/false);

  std::shared_ptr<CompilerInvocation> inv = invocation(job.args, diags, fs);
  if (!inv) {
    send(blaze_wire::done(true));
    return;
  }

  if (job.mode == blaze_wire::Mode::Compile) {
    std::string object;
    EmitObjAction emit;
    bool ok = parse(inv, fs, consumer, emit, llvm::makeIntrusiveRefCnt<MemoryBackend>(object));
    if (!ok || consumer.sent_errors > 0) {
      send(blaze_wire::done(true));
      return;
    }
    if (object.empty()) stop(FrontEnd); // job.rs: NoOutput, named there
    send_out(blaze_wire::OutKind::Object, object);
    send(blaze_wire::done(false));
    return;
  }

  // Declarations: the macros first (only errors said: the second parse says the rest once),
  // then the parse whose main file holds the probes.
  std::vector<MacroSeen> macros;
  fs->set_main(std::string());
  consumer.errors_only = true;
  {
    MacroScan scan(macros);
    bool ok = parse(inv, fs, consumer, scan, nullptr);
    if (!ok || consumer.sent_errors > 0) {
      send(blaze_wire::done(true));
      return;
    }
  }
  consumer.errors_only = false;
  fs->set_main(probes(macros));
  std::string doc;
  {
    DumpAction dump(macros, doc);
    bool ok = parse(inv, fs, consumer, dump, nullptr);
    if (!ok && consumer.sent_errors == 0) stop(FrontEnd);
  }
  if (consumer.sent_errors > 0) {
    send(blaze_wire::done(true));
    return;
  }
  send_out(blaze_wire::OutKind::Decls, doc);
  send(blaze_wire::done(false));
}

} // namespace

int main(int argc, char **argv) {
  if (argc != 2 || llvm::StringRef(argv[1]) != "--serve") stop(BadJob);
  signal(SIGPIPE, SIG_IGN);
#if defined(__APPLE__)
  {
    int on = 1;
    setsockopt(CHANNEL, SOL_SOCKET, SO_NOSIGPIPE, &on, sizeof on);
  }
#endif
  // Before confinement nothing is opened either: LLVM's targets are registered from static
  // tables.
  llvm::InitializeAllTargetInfos();
  llvm::InitializeAllTargets();
  llvm::InitializeAllTargetMCs();
  llvm::InitializeAllAsmPrinters();

  blaze_wire::Confinement confinement = confine();
  send(blaze_wire::hello(confinement));

  std::vector<uint8_t> body = receive();
  std::optional<blaze_wire::Job> job = blaze_wire::read_job(body.data(), body.size());
  if (!job) stop(BadJob);
  run(*job);
  return 0;
}

// The clang API relied on (clang 23; UNSURE: read from memory of clang 20-22, not from headers):
//   driver::Driver(StringRef ClangExecutable, StringRef TargetTriple, DiagnosticsEngine&,
//     std::string Title, IntrusiveRefCntPtr<vfs::FileSystem>); BuildCompilation(ArrayRef<const
//     char*>); Compilation::getJobs, containsError; Command::getCreator().getName(),
//     getArguments(); setCheckInputsExist.
//   CompilerInvocation::CreateFromArgs(CompilerInvocation&, ArrayRef<const char*>,
//     DiagnosticsEngine&, const char *Argv0).
//   CompilerInstance(std::shared_ptr<CompilerInvocation>); setVirtualFileSystem;
//     createDiagnostics(DiagnosticConsumer*, bool) (clang 21+ takes the file system first in
//     some versions: createDiagnostics(vfs::FileSystem&, ...)); createFileManager();
//     setOutputBackend(IntrusiveRefCntPtr<vfs::OutputBackend>) and that createDefaultOutputFile
//     writes through it (the alternative is EmitLLVMOnlyAction + emitBackendOutput into a
//     raw_svector_ostream); ExecuteAction.
//   DiagnosticsEngine(IntrusiveRefCntPtr<DiagnosticIDs>, DiagnosticOptions&, DiagnosticConsumer*,
//     bool) (DiagnosticOptions by reference since clang 21); DiagnosticIDs::create().
//   Diagnostic::FormatDiagnostic, getID, getLocation, hasSourceManager.
//   PreprocessorFrontendAction, ASTFrontendAction, EmitObjAction; Preprocessor::macros(),
//     getMacroInfo, EnterMainSourceFile, Lex; MacroInfo::isUsedForHeaderGuard,
//     getDefinitionLoc, getDefinitionEndLoc, tokens(), isFunctionLike, getNumTokens.
//   SourceManager::getFileEntryRefForID, getExpansionLoc, getFileLoc, isWrittenInBuiltinFile,
//     isWrittenInCommandLineFile; Lexer::getLocForEndOfToken.
//   ASTContext::getASTRecordLayout, getBuiltinVaListDecl, getBuiltinVaListType,
//     getQualifiedType, getAsArrayType; ASTRecordLayout::getFieldOffset, getSize, getAlignment,
//     getBaseClassOffset, getVBaseClassOffset; FieldDecl::getBitWidthValue() (no ASTContext
//     argument since clang 20), isZeroLengthBitField() (likewise); ConstantArrayType::getZExtSize;
//     Type::getAsTagDecl, getLocallyUnqualifiedSingleStepDesugaredType; TypedefType::getDecl
//     (clang 22 renamed TagType's getDecl to getOriginalDecl; getAsTagDecl is used for tags so
//     only TypedefType's is relied on); BTFTagAttributedType, CountAttributedType exist;
//     StringLiteral::isOrdinary, getBytes; Expr::EvaluateAsInt(EvalResult&, const ASTContext&),
//     EvaluateAsFloat, isNullPointerConstant, containsErrors; FunctionDecl::isNoReturn,
//     isInlineDefinitionExternallyVisible, getBuiltinID, isExternC; VarDecl::getDefinition(),
//     getActingDefinition(), getTLSKind, isFileVarDecl; AsmLabelAttr::getLabel;
//     MaxFieldAlignmentAttr::getAlignment (bits).
