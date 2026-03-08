/* Detect platform triplet from builtin defines
 * cc -E Misc/platform_triplet.c | grep '^PLATFORM_TRIPLET=' | tr -d ' '
 *
 * Also detects the closest Rust target triple for cargo/rustc:
 * cc -E Misc/platform_triplet.c | grep '^RUST_TARGET=' | tr -d ' '
 *
 * And the LLVM/clang target triple (for bindgen), only when it differs
 * from RUST_TARGET:
 * cc -E Misc/platform_triplet.c | grep '^LLVM_TARGET=' | tr -d ' '
 */
#undef bfin
#undef cris
#undef fr30
#undef linux
#undef hppa
#undef hpux
#undef i386
#undef mips
#undef powerpc
#undef sparc
#undef unix

#if defined(__ANDROID__)
#  if defined(__x86_64__)
PLATFORM_TRIPLET=x86_64-linux-android
RUST_TARGET=x86_64-linux-android
#  elif defined(__i386__)
PLATFORM_TRIPLET=i686-linux-android
RUST_TARGET=i686-linux-android
#  elif defined(__aarch64__)
PLATFORM_TRIPLET=aarch64-linux-android
RUST_TARGET=aarch64-linux-android
#  elif defined(__arm__)
PLATFORM_TRIPLET=arm-linux-androideabi
RUST_TARGET=arm-linux-androideabi
#  else
#    error unknown Android platform
#  endif

#elif defined(__linux__)
/*
 * BEGIN of Linux block
 */
// Detect libc (based on config.guess)
# include <features.h>
# if defined(__UCLIBC__)
#  error uclibc not supported
# elif defined(__dietlibc__)
#  error dietlibc not supported
# elif defined(__GLIBC__)
#  define LIBC gnu
#  define LIBC_X32 gnux32
#  define RUST_LIBC gnu
#  define RUST_LIBC_X32 gnux32
#  if defined(__ARM_PCS_VFP)
#   define LIBC_ARM gnueabihf
#   define RUST_LIBC_ARM gnueabihf
#  else
#   define LIBC_ARM gnueabi
#   define RUST_LIBC_ARM gnueabi
#  endif
#  if defined(__loongarch__)
#   if defined(__loongarch_soft_float)
#    define LIBC_LA gnusf
#    define RUST_LIBC_LA unknown
#   elif defined(__loongarch_single_float)
#    define LIBC_LA gnuf32
#    define RUST_LIBC_LA unknown
#   elif defined(__loongarch_double_float)
#    define LIBC_LA gnu
#    define RUST_LIBC_LA gnu
#   else
#    error unknown loongarch floating-point base abi
#   endif
#  endif
#  if defined(_MIPS_SIM)
#   if defined(__mips_hard_float)
#    if defined(_ABIO32) && _MIPS_SIM == _ABIO32
#     define LIBC_MIPS gnu
#     define RUST_LIBC_MIPS gnu
#    elif defined(_ABIN32) && _MIPS_SIM == _ABIN32
#     define LIBC_MIPS gnuabin32
#     define RUST_LIBC_MIPS unknown
#    elif defined(_ABI64) && _MIPS_SIM == _ABI64
#     define LIBC_MIPS gnuabi64
#     define RUST_LIBC_MIPS gnuabi64
#    else
#     error unknown mips sim value
#    endif
#   else
#    if defined(_ABIO32) && _MIPS_SIM == _ABIO32
#     define LIBC_MIPS gnusf
#     define RUST_LIBC_MIPS unknown
#    elif defined(_ABIN32) && _MIPS_SIM == _ABIN32
#     define LIBC_MIPS gnuabin32sf
#     define RUST_LIBC_MIPS unknown
#    elif defined(_ABI64) && _MIPS_SIM == _ABI64
#     define LIBC_MIPS gnuabi64sf
#     define RUST_LIBC_MIPS unknown
#    else
#     error unknown mips sim value
#    endif
#   endif
#  endif
#  if defined(__SPE__)
#   define LIBC_PPC gnuspe
#   define RUST_LIBC_PPC gnuspe
#  else
#   define LIBC_PPC gnu
#   define RUST_LIBC_PPC gnu
#  endif
# else
// Heuristic to detect musl libc
#  include <stdarg.h>
#  ifdef __DEFINED_va_list
#   define LIBC musl
#   define LIBC_X32 muslx32
#   define RUST_LIBC musl
#   define RUST_LIBC_X32 unknown
#   if defined(__ARM_PCS_VFP)
#    define LIBC_ARM musleabihf
#    define RUST_LIBC_ARM musleabihf
#   else
#    define LIBC_ARM musleabi
#    define RUST_LIBC_ARM musleabi
#   endif
#   if defined(__loongarch__)
#    if defined(__loongarch_soft_float)
#     define LIBC_LA muslsf
#     define RUST_LIBC_LA unknown
#    elif defined(__loongarch_single_float)
#     define LIBC_LA muslf32
#     define RUST_LIBC_LA unknown
#    elif defined(__loongarch_double_float)
#     define LIBC_LA musl
#     define RUST_LIBC_LA musl
#    else
#     error unknown loongarch floating-point base abi
#    endif
#   endif
#   if defined(_MIPS_SIM)
#    if defined(__mips_hard_float)
#     if defined(_ABIO32) && _MIPS_SIM == _ABIO32
#      define LIBC_MIPS musl
#      define RUST_LIBC_MIPS musl
#     elif defined(_ABIN32) && _MIPS_SIM == _ABIN32
#      define LIBC_MIPS musln32
#      define RUST_LIBC_MIPS unknown
#     elif defined(_ABI64) && _MIPS_SIM == _ABI64
#      define LIBC_MIPS musl
#      define RUST_LIBC_MIPS muslabi64
#      define LLVM_LIBC_MIPS musl
#     else
#      error unknown mips sim value
#     endif
#    else
#     if defined(_ABIO32) && _MIPS_SIM == _ABIO32
#      define LIBC_MIPS muslsf
#      define RUST_LIBC_MIPS unknown
#     elif defined(_ABIN32) && _MIPS_SIM == _ABIN32
#      define LIBC_MIPS musln32sf
#      define RUST_LIBC_MIPS unknown
#     elif defined(_ABI64) && _MIPS_SIM == _ABI64
#      define LIBC_MIPS muslsf
#      define RUST_LIBC_MIPS unknown
#     else
#      error unknown mips sim value
#     endif
#    endif
#   endif
#   if defined(_SOFT_FLOAT) || defined(__NO_FPRS__)
#    define LIBC_PPC muslsf
#    define RUST_LIBC_PPC unknown
#   else
#    define LIBC_PPC musl
#    define RUST_LIBC_PPC musl
#   endif
#  else
#   error unknown libc
#  endif
# endif

# if defined(__x86_64__) && defined(__LP64__)
PLATFORM_TRIPLET=x86_64-linux-LIBC
RUST_TARGET=x86_64-unknown-linux-RUST_LIBC
# elif defined(__x86_64__) && defined(__ILP32__)
PLATFORM_TRIPLET=x86_64-linux-LIBC_X32
RUST_TARGET=x86_64-unknown-linux-RUST_LIBC_X32
# elif defined(__i386__)
PLATFORM_TRIPLET=i386-linux-LIBC
RUST_TARGET=i686-unknown-linux-RUST_LIBC
# elif defined(__aarch64__) && defined(__AARCH64EL__)
#  if defined(__ILP32__)
PLATFORM_TRIPLET=aarch64_ilp32-linux-LIBC
RUST_TARGET=unknown
#  else
PLATFORM_TRIPLET=aarch64-linux-LIBC
RUST_TARGET=aarch64-unknown-linux-RUST_LIBC
#  endif
# elif defined(__aarch64__) && defined(__AARCH64EB__)
#  if defined(__ILP32__)
PLATFORM_TRIPLET=aarch64_be_ilp32-linux-LIBC
RUST_TARGET=unknown
#  else
PLATFORM_TRIPLET=aarch64_be-linux-LIBC
RUST_TARGET=aarch64_be-unknown-linux-RUST_LIBC
#  endif
# elif defined(__alpha__)
PLATFORM_TRIPLET=alpha-linux-LIBC
RUST_TARGET=unknown
# elif defined(__ARM_EABI__)
#  if defined(__ARMEL__)
PLATFORM_TRIPLET=arm-linux-LIBC_ARM
RUST_TARGET=arm-unknown-linux-RUST_LIBC_ARM
#  else
PLATFORM_TRIPLET=armeb-linux-LIBC_ARM
RUST_TARGET=armeb-unknown-linux-RUST_LIBC_ARM
#  endif
# elif defined(__hppa__)
PLATFORM_TRIPLET=hppa-linux-LIBC
RUST_TARGET=unknown
# elif defined(__ia64__)
PLATFORM_TRIPLET=ia64-linux-LIBC
RUST_TARGET=unknown
# elif defined(__loongarch__) && defined(__loongarch_lp64)
PLATFORM_TRIPLET=loongarch64-linux-LIBC_LA
RUST_TARGET=loongarch64-unknown-linux-RUST_LIBC_LA
# elif defined(__m68k__) && !defined(__mcoldfire__)
PLATFORM_TRIPLET=m68k-linux-LIBC
RUST_TARGET=m68k-unknown-linux-RUST_LIBC
# elif defined(__mips__)
#  if defined(__mips_isa_rev) && (__mips_isa_rev >=6)
#   if defined(_MIPSEL) && defined(__mips64)
PLATFORM_TRIPLET=mipsisa64r6el-linux-LIBC_MIPS
RUST_TARGET=mipsisa64r6el-unknown-linux-RUST_LIBC_MIPS
#   elif defined(_MIPSEL)
PLATFORM_TRIPLET=mipsisa32r6el-linux-LIBC_MIPS
RUST_TARGET=mipsisa32r6el-unknown-linux-RUST_LIBC_MIPS
#   elif defined(__mips64)
PLATFORM_TRIPLET=mipsisa64r6-linux-LIBC_MIPS
RUST_TARGET=mipsisa64r6-unknown-linux-RUST_LIBC_MIPS
#   else
PLATFORM_TRIPLET=mipsisa32r6-linux-LIBC_MIPS
RUST_TARGET=mipsisa32r6-unknown-linux-RUST_LIBC_MIPS
#   endif
#  else
#   if defined(_MIPSEL) && defined(__mips64)
PLATFORM_TRIPLET=mips64el-linux-LIBC_MIPS
RUST_TARGET=mips64el-unknown-linux-RUST_LIBC_MIPS
#    ifdef LLVM_LIBC_MIPS
LLVM_TARGET=mips64el-unknown-linux-LLVM_LIBC_MIPS
#    endif
#   elif defined(_MIPSEL)
PLATFORM_TRIPLET=mipsel-linux-LIBC_MIPS
RUST_TARGET=mipsel-unknown-linux-RUST_LIBC_MIPS
#   elif defined(__mips64)
PLATFORM_TRIPLET=mips64-linux-LIBC_MIPS
RUST_TARGET=mips64-unknown-linux-RUST_LIBC_MIPS
#    ifdef LLVM_LIBC_MIPS
LLVM_TARGET=mips64-unknown-linux-LLVM_LIBC_MIPS
#    endif
#   else
PLATFORM_TRIPLET=mips-linux-LIBC_MIPS
RUST_TARGET=mips-unknown-linux-RUST_LIBC_MIPS
#   endif
#  endif
# elif defined(__or1k__)
PLATFORM_TRIPLET=or1k-linux-LIBC
RUST_TARGET=unknown
# elif defined(__powerpc64__)
#  if defined(__LITTLE_ENDIAN__)
PLATFORM_TRIPLET=powerpc64le-linux-LIBC
RUST_TARGET=powerpc64le-unknown-linux-RUST_LIBC
#  else
PLATFORM_TRIPLET=powerpc64-linux-LIBC
RUST_TARGET=powerpc64-unknown-linux-RUST_LIBC
#  endif
# elif defined(__powerpc__)
PLATFORM_TRIPLET=powerpc-linux-LIBC_PPC
RUST_TARGET=powerpc-unknown-linux-RUST_LIBC_PPC
# elif defined(__s390x__)
PLATFORM_TRIPLET=s390x-linux-LIBC
RUST_TARGET=s390x-unknown-linux-RUST_LIBC
# elif defined(__s390__)
PLATFORM_TRIPLET=s390-linux-LIBC
RUST_TARGET=unknown
# elif defined(__sh__) && defined(__LITTLE_ENDIAN__)
PLATFORM_TRIPLET=sh4-linux-LIBC
RUST_TARGET=unknown
# elif defined(__sparc__) && defined(__arch64__)
PLATFORM_TRIPLET=sparc64-linux-LIBC
RUST_TARGET=sparc64-unknown-linux-RUST_LIBC
# elif defined(__sparc__)
PLATFORM_TRIPLET=sparc-linux-LIBC
RUST_TARGET=sparc-unknown-linux-RUST_LIBC
# elif defined(__riscv)
#  if __riscv_xlen == 32
PLATFORM_TRIPLET=riscv32-linux-LIBC
RUST_TARGET=riscv32gc-unknown-linux-RUST_LIBC
LLVM_TARGET=riscv32-unknown-linux-RUST_LIBC
#  elif __riscv_xlen == 64
PLATFORM_TRIPLET=riscv64-linux-LIBC
RUST_TARGET=riscv64gc-unknown-linux-RUST_LIBC
LLVM_TARGET=riscv64-unknown-linux-RUST_LIBC
#  else
#   error unknown platform triplet
#  endif
# else
#   error unknown platform triplet
# endif
/*
 * END of Linux block
 */
#elif defined(__FreeBSD_kernel__)
# if defined(__LP64__)
PLATFORM_TRIPLET=x86_64-kfreebsd-gnu
RUST_TARGET=unknown
# elif defined(__i386__)
PLATFORM_TRIPLET=i386-kfreebsd-gnu
RUST_TARGET=unknown
# else
#   error unknown platform triplet
# endif
#elif defined(__gnu_hurd__)
# if defined(__x86_64__) && defined(__LP64__)
PLATFORM_TRIPLET=x86_64-gnu
RUST_TARGET=x86_64-unknown-hurd-gnu
# elif defined(__i386__)
PLATFORM_TRIPLET=i386-gnu
RUST_TARGET=i686-unknown-hurd-gnu
# else
#   error unknown platform triplet
# endif
#elif defined(__APPLE__)
#  include "TargetConditionals.h"
// Older macOS SDKs do not define TARGET_OS_*
#  if defined(TARGET_OS_IOS) && TARGET_OS_IOS
#    if defined(TARGET_OS_SIMULATOR) && TARGET_OS_SIMULATOR
#      if __x86_64__
PLATFORM_TRIPLET=x86_64-iphonesimulator
RUST_TARGET=x86_64-apple-ios
LLVM_TARGET=x86_64-apple-ios-simulator
#      else
PLATFORM_TRIPLET=arm64-iphonesimulator
RUST_TARGET=aarch64-apple-ios-sim
LLVM_TARGET=arm64-apple-ios-simulator
#      endif
#    else
PLATFORM_TRIPLET=arm64-iphoneos
RUST_TARGET=aarch64-apple-ios
LLVM_TARGET=arm64-apple-ios
#    endif
// Older macOS SDKs do not define TARGET_OS_OSX
#  elif !defined(TARGET_OS_OSX) || TARGET_OS_OSX
PLATFORM_TRIPLET=darwin
#    if defined(__x86_64__)
RUST_TARGET=x86_64-apple-darwin
LLVM_TARGET=x86_64-apple-macosx
#    elif defined(__aarch64__)
RUST_TARGET=aarch64-apple-darwin
LLVM_TARGET=arm64-apple-macosx
#    else
RUST_TARGET=unknown
#    endif
#  else
#    error unknown Apple platform
#  endif
#elif defined(__VXWORKS__)
PLATFORM_TRIPLET=vxworks
RUST_TARGET=unknown
#elif defined(__wasm32__)
#  if defined(__EMSCRIPTEN__)
PLATFORM_TRIPLET=wasm32-emscripten
RUST_TARGET=wasm32-unknown-emscripten
#  elif defined(__wasi__)
#    if defined(_REENTRANT)
PLATFORM_TRIPLET=wasm32-wasi-threads
RUST_TARGET=wasm32-wasip1-threads
LLVM_TARGET=wasm32-wasi
#    else
PLATFORM_TRIPLET=wasm32-wasi
RUST_TARGET=wasm32-wasip1
#    endif
#  else
#    error unknown wasm32 platform
#  endif
#elif defined(__wasm64__)
#  if defined(__EMSCRIPTEN__)
PLATFORM_TRIPLET=wasm64-emscripten
RUST_TARGET=unknown
#  elif defined(__wasi__)
PLATFORM_TRIPLET=wasm64-wasi
RUST_TARGET=unknown
#  else
#    error unknown wasm64 platform
#  endif
#else
# error unknown platform triplet
#endif
