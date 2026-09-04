# sgc-drm-c / sgc-drm-cpp: minimal C and C++ sample clients for libsgc.
#
# They link the C ABI statically: libsgc.a is taken from the libsgc-c repo's
# cargo target dir, debug or release per the meson buildtype, plus the
# aarch64 triple when cross-building (host debug: `cargo build -p libsgc-c`;
# release: `cargo build --release -p libsgc-c`; board: cross-build in the
# libsgc-c repo with the gnu target). The samples themselves only need the
# kernel DRM UAPI headers (linux-libc-dev) — the drawing is raw ioctls
# (drmdraw.c), no GBM/EGL, no libdrm linked.
#
# Host:   meson setup build && meson compile -C build
# Board:  meson setup build-aarch64 --cross-file=aarch64-cross.txt \
#           -Dbuildtype=release -Dsgc_dir=/abs/path/to/libsgc-c
project('sgc-c-samples', 'c', 'cpp',
  version : '0.1.0',
  license : 'Unlicense',
  default_options : ['c_std=gnu99', 'cpp_std=c++17', 'warning_level=2'])

sgc_dir = get_option('sgc_dir')
if sgc_dir == ''
  error('libsgc not found: reconfigure with -Dsgc_dir=/path/to/libsgc-c')
endif
if not sgc_dir.startswith('/')
  sgc_dir = join_paths(meson.current_source_dir(), sgc_dir)
endif
sgc_target_dir = meson.is_cross_build() ? 'aarch64-unknown-linux-gnu' : ''
sgc_profile = get_option('buildtype') == 'release' ? 'release' : 'debug'
sgc_libdir = join_paths(sgc_dir, 'target', sgc_target_dir, sgc_profile)
sgc_inc = join_paths(sgc_dir, 'include')

# libsgc sits outside this project's tree, so bypass meson's include-dir
# rules: plain -I for the header, and the .a linked by absolute path (this
# also pins the static archive — no .so to get picked up).
add_project_arguments('-I' + sgc_inc, language : ['c', 'cpp'])
dep_sgc = declare_dependency(
  link_args : [join_paths(sgc_libdir, 'libsgc.a'),
               '-lpthread', '-ldl', '-lm'],
)
message('libsgc: ' + join_paths(sgc_libdir, 'libsgc.a'))

drmdraw = static_library('drmdraw', 'drmdraw.c')
drmdraw_dep = declare_dependency(
  link_with : drmdraw,
  include_directories : include_directories('.'),
)

executable('sgc-drm-c', 'main.c', dependencies : [dep_sgc, drmdraw_dep])
executable('sgc-drm-cpp', 'main.cc', dependencies : [dep_sgc, drmdraw_dep])
