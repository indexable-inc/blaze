# blaze-cfront: the C toolchain helper (D174, docs/v2/plan/design/c.md "The toolchain side").
# Built by .github/scripts/llvm.py (`"cfront": true`) against the blaze LLVM prefix it just
# installed, with the clang libraries, headers and CMake exports that build adds
# (CFRONT_COMPONENTS); installed as <prefix>/bin/blaze-cfront and pinned in pins/cfront.json.
#
# The helper is blaze's one piece of non-Rust code (c.md decision 2): the file-system adapter,
# the argument hand-off and the declaration dump. It links clang and LLVM statically, so the
# archive carries the helper and not the libraries.

cmake_minimum_required(VERSION 3.20)
project(blaze_cfront LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

# llvm.py passes CMAKE_PREFIX_PATH=<prefix>; nothing else is searched.
find_package(LLVM REQUIRED CONFIG NO_DEFAULT_PATH PATHS "${CMAKE_PREFIX_PATH}/lib/cmake/llvm")
find_package(Clang REQUIRED CONFIG NO_DEFAULT_PATH PATHS "${CMAKE_PREFIX_PATH}/lib/cmake/clang")

if(NOT LLVM_VERSION_MAJOR EQUAL 23)
  message(FATAL_ERROR "blaze-cfront: LLVM ${LLVM_VERSION_MAJOR} found; flags.rs CLANG_MAJOR is 23")
endif()

add_executable(blaze-cfront main.cpp)
target_include_directories(blaze-cfront PRIVATE ${LLVM_INCLUDE_DIRS} ${CLANG_INCLUDE_DIRS})
separate_arguments(LLVM_DEFINITIONS_LIST NATIVE_COMMAND ${LLVM_DEFINITIONS})
target_compile_definitions(blaze-cfront PRIVATE ${LLVM_DEFINITIONS_LIST})
# LLVM is built without RTTI and exceptions; the helper matches.
target_compile_options(blaze-cfront PRIVATE -fno-rtti -fno-exceptions)

# What the helper uses of LLVM: the targets the pinned clang emits for (code generation and the
# MC layer, through clangCodeGen), the option tables the driver parses with, and Support.
llvm_map_components_to_libnames(BLAZE_CFRONT_LLVM_LIBS
  AArch64 X86 Core Support Option MC TargetParser BitWriter IRReader Passes)

target_link_libraries(blaze-cfront PRIVATE
  clangFrontend   # CompilerInstance, CompilerInvocation, FrontendAction
  clangCodeGen    # EmitLLVMOnlyAction, emitBackendOutput
  clangDriver     # Driver: the user's arguments to one cc1 invocation
  clangParse
  clangSema
  clangAnalysis
  clangAST        # ASTContext, ASTRecordLayout, the declarations walked
  clangASTMatchers
  clangEdit
  clangLex        # Preprocessor, MacroInfo
  clangBasic      # Diagnostics, SourceManager, FileManager over the wire's FileSystem
  clangSerialization
  clangSupport
  ${BLAZE_CFRONT_LLVM_LIBS})

if(APPLE)
  # sandbox_init (libSystem's libsandbox): the Seatbelt profile the helper confines itself with.
  target_link_libraries(blaze-cfront PRIVATE "-lsandbox")
endif()

install(TARGETS blaze-cfront RUNTIME DESTINATION bin)
