use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::{env, fs};

fn toyos_cc() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_toyos-cc"))
}

fn testcases_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testcases/tinycc")
}

/// Path to the toyos-libc directory (sibling of toyos-cc).
fn libc_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("libc")
}

/// Build toyos-libc for a specific target triple and return path to the .rlib.
/// Results are cached per-target. Uses `+nightly` to avoid the toyos toolchain
/// override in the userland directory.
fn libc_archive(target: &str) -> PathBuf {
    static CACHE: LazyLock<Mutex<HashMap<String, PathBuf>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut cache = CACHE.lock().unwrap();
    if let Some(path) = cache.get(target) {
        return path.clone();
    }
    let libc = libc_dir();
    let target_dir = libc.join("target");
    let output = Command::new("cargo")
        .args(["+nightly", "rustc", "--release", "--target", target, "--crate-type", "staticlib"])
        .arg("--manifest-path")
        .arg(libc.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .output()
        .expect("failed to build toyos-libc");
    assert!(
        output.status.success(),
        "toyos-libc build for {target} failed:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    let path = target_dir.join(format!("{target}/release/libtoyos_libc.a"));
    assert!(path.exists(), "expected staticlib at {}", path.display());
    cache.insert(target.to_string(), path.clone());
    path
}

fn host_target() -> &'static str {
    if cfg!(target_arch = "aarch64") && cfg!(target_os = "macos") {
        "aarch64-apple-darwin"
    } else if cfg!(target_arch = "x86_64") && cfg!(target_os = "macos") {
        "x86_64-apple-darwin"
    } else if cfg!(target_arch = "x86_64") && cfg!(target_os = "linux") {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(target_arch = "aarch64") && cfg!(target_os = "linux") {
        "aarch64-unknown-linux-gnu"
    } else {
        panic!("unsupported host for toyos-libc test")
    }
}

fn libc_include_args() -> Vec<String> {
    vec!["-I".to_string(), libc_dir().join("include").to_string_lossy().to_string()]
}

/// Run a test with an optional cross-compilation target.
/// When `target` is Some, compiles for that target and runs accordingly.
fn run_test_with_target(name: &str, args: &[&str], target: Option<&str>) {
    let label = target.unwrap_or("native");
    let dir = testcases_dir();
    let c_file = dir.join(format!("{name}.c"));
    let expect_file = dir.join(format!("{name}.expect"));

    assert!(c_file.exists(), "missing test file: {}", c_file.display());
    assert!(expect_file.exists(), "missing expect file: {}", expect_file.display());

    let expected = fs::read_to_string(&expect_file).unwrap();

    let suffix = target.map_or("".to_string(), |t| format!("-{t}"));
    let pid = std::process::id();
    let tmp = env::temp_dir().join(format!("toyos-cc-test{suffix}-{name}-{pid}"));
    let obj = tmp.with_extension("o");
    let bin = tmp.with_extension("bin");

    // Compile (run from testcases dir so __FILE__ uses relative path)
    let mut compile_cmd = Command::new(toyos_cc());
    compile_cmd.current_dir(&dir);
    if let Some(t) = target {
        compile_cmd.args(["--target", t]);
    }
    compile_cmd
        .args(["-c", "-o"])
        .arg(&obj)
        .args(libc_include_args())
        .arg("-I")
        .arg(&dir)
        .arg(format!("{name}.c"));

    let compile = compile_cmd.output().expect("failed to run toyos-cc");
    assert!(
        compile.status.success(),
        "toyos-cc ({label}) failed to compile {name}.c:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );

    // Compile companion files (e.g., "104+_inline.c" for "104_inline")
    let mut extra_objs = Vec::new();
    if let Some(idx) = name.find('_') {
        let prefix = &name[..idx];
        let file_suffix = &name[idx..];
        let companion_name = format!("{}+{}.c", prefix, file_suffix);
        let companion = dir.join(&companion_name);
        if companion.exists() {
            let extra_obj = tmp.with_extension("extra.o");
            let mut extra_cmd = Command::new(toyos_cc());
            extra_cmd.current_dir(&dir);
            if let Some(t) = target {
                extra_cmd.args(["--target", t]);
            }
            extra_cmd
                .args(["-c", "-o"])
                .arg(&extra_obj)
                .args(libc_include_args())
                .arg("-I")
                .arg(&dir)
                .arg(&companion);
            let cc_compile = extra_cmd.output().expect("failed to compile companion file");
            assert!(
                cc_compile.status.success(),
                "toyos-cc ({label}) failed to compile companion {companion_name}:\nstderr: {}",
                String::from_utf8_lossy(&cc_compile.stderr),
            );
            extra_objs.push(extra_obj);
        }
    }

    // Link
    let libc_target = target.unwrap_or(host_target());
    let libc_path = libc_archive(libc_target);
    let libc_data = fs::read(&libc_path).unwrap();
    let mut objects: Vec<(String, Vec<u8>)> = vec![(obj.display().to_string(), fs::read(&obj).unwrap())];
    for extra in &extra_objs {
        objects.push((extra.display().to_string(), fs::read(extra).unwrap()));
    }
    objects.push((libc_path.display().to_string(), libc_data));
    let _ = fs::remove_file(&obj);
    for extra in &extra_objs {
        let _ = fs::remove_file(extra);
    }

    let is_macho = target.map_or(cfg!(target_os = "macos"), |t| t.contains("apple"));
    let entry = if is_macho { "_main" } else { "main" };
    let linked = if is_macho {
        toyos_ld::link_macho(&objects, entry, true)
    } else {
        toyos_ld::link_full(&objects, entry, true, false)
    };
    let linked = linked.unwrap_or_else(|e| panic!("toyos-ld ({label}) failed for {name}: {e}"));
    fs::write(&bin, &linked).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Run (use arch -x86_64 for x86_64 cross-compilation on ARM64 Mac)
    let is_x86_64_cross = target.map_or(false, |t| t.starts_with("x86_64"));
    let mut cmd = if is_x86_64_cross {
        let mut c = Command::new("arch");
        c.args(["-x86_64"]).arg(&bin);
        c
    } else {
        Command::new(&bin)
    };
    cmd.args(args).current_dir(&dir);

    let run = cmd.output().unwrap_or_else(|e| {
        panic!("failed to run {name} ({label}): {e}");
    });

    let _ = fs::remove_file(&bin);

    let actual = String::from_utf8_lossy(&run.stdout);

    assert!(
        run.status.success(),
        "test binary {name} ({label}) failed with status {}:\nstdout: {}\nstderr: {}",
        run.status,
        actual,
        String::from_utf8_lossy(&run.stderr),
    );

    assert_eq!(
        actual.trim_end(),
        expected.trim_end(),
        "output mismatch for {name} ({label})\n--- expected ---\n{}\n--- actual ---\n{}",
        expected.trim_end(),
        actual.trim_end(),
    );
}

fn run_test(name: &str, args: &[&str]) {
    run_test_with_target(name, args, None);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn run_test_x86_64(name: &str, args: &[&str]) {
    run_test_with_target(name, args, Some("x86_64-apple-darwin"));
}

/// Each invocation generates a native test plus (on macOS ARM64) an x86_64
/// Rosetta cross-compilation test in a submodule with the same name.
macro_rules! tinycc_test {
    ($rust_name:ident, $file:expr) => {
        #[test]
        fn $rust_name() {
            run_test($file, &[]);
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        mod $rust_name {
            #[test]
            fn x86_64() {
                super::run_test_x86_64($file, &[]);
            }
        }
    };
    ($rust_name:ident, $file:expr, native_only) => {
        #[test]
        fn $rust_name() {
            run_test($file, &[]);
        }
    };
    ($rust_name:ident, $file:expr, args: [$($arg:expr),*]) => {
        #[test]
        fn $rust_name() {
            run_test($file, &[$($arg),*]);
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        mod $rust_name {
            #[test]
            fn x86_64() {
                super::run_test_x86_64($file, &[$($arg),*]);
            }
        }
    };
}

tinycc_test!(t00_assignment, "00_assignment");
tinycc_test!(t01_comment, "01_comment");
tinycc_test!(t02_printf, "02_printf");
tinycc_test!(t04_for, "04_for");
tinycc_test!(t05_array, "05_array");
tinycc_test!(t06_case, "06_case");
tinycc_test!(t07_function, "07_function");
tinycc_test!(t08_while, "08_while");
tinycc_test!(t09_do_while, "09_do_while");
tinycc_test!(t10_pointer, "10_pointer");
tinycc_test!(t11_precedence, "11_precedence");
tinycc_test!(t12_hashdefine, "12_hashdefine");
tinycc_test!(t13_integer_literals, "13_integer_literals");
tinycc_test!(t14_if, "14_if");
tinycc_test!(t15_recursion, "15_recursion");
tinycc_test!(t16_nesting, "16_nesting");
tinycc_test!(t17_enum, "17_enum");
tinycc_test!(t18_include, "18_include");
tinycc_test!(t19_pointer_arithmetic, "19_pointer_arithmetic");
tinycc_test!(t20_pointer_comparison, "20_pointer_comparison");
tinycc_test!(t21_char_array, "21_char_array");
// x86_64 `long double` is 80-bit; our compiler uses 64-bit double — skip x86_64.
tinycc_test!(t22_floating_point, "22_floating_point", native_only);
tinycc_test!(t23_type_coercion, "23_type_coercion");
tinycc_test!(t24_math_library, "24_math_library");
tinycc_test!(t25_quicksort, "25_quicksort");
tinycc_test!(t26_character_constants, "26_character_constants");
tinycc_test!(t27_sizeof, "27_sizeof");
tinycc_test!(t28_strings, "28_strings");
tinycc_test!(t29_array_address, "29_array_address");
tinycc_test!(t30_hanoi, "30_hanoi");
tinycc_test!(t31_args, "31_args", args: ["arg1", "arg2", "arg3", "arg4", "arg5"]);
tinycc_test!(t32_led, "32_led", args: ["12345"]);
// t33_ternary_op uses _Generic which is not implemented yet
// tinycc_test!(t33_ternary_op, "33_ternary_op");
tinycc_test!(t34_array_assignment, "34_array_assignment");
tinycc_test!(t35_sizeof, "35_sizeof");
tinycc_test!(t36_array_initialisers, "36_array_initialisers");
tinycc_test!(t37_sprintf, "37_sprintf");
tinycc_test!(t38_multiple_array_index, "38_multiple_array_index");
tinycc_test!(t39_typedef, "39_typedef");
tinycc_test!(t40_stdio, "40_stdio");
tinycc_test!(t41_hashif, "41_hashif");
tinycc_test!(t42_function_pointer, "42_function_pointer");
tinycc_test!(t43_void_param, "43_void_param");
tinycc_test!(t44_scoped_declarations, "44_scoped_declarations");
tinycc_test!(t45_empty_for, "45_empty_for");
tinycc_test!(t46_grep, "46_grep", args: ["[^* ]*[:a:d: ]+\\:\\*-/: $", "46_grep.c"]);
tinycc_test!(t47_switch_return, "47_switch_return");
tinycc_test!(t48_nested_break, "48_nested_break");
tinycc_test!(t49_bracket_evaluation, "49_bracket_evaluation");
tinycc_test!(t50_logical_second_arg, "50_logical_second_arg");
tinycc_test!(t51_static, "51_static");
tinycc_test!(t52_unnamed_enum, "52_unnamed_enum");
tinycc_test!(t54_goto, "54_goto");
tinycc_test!(t55_lshift_type, "55_lshift_type");
tinycc_test!(t61_integers, "61_integers");
tinycc_test!(t64_macro_nesting, "64_macro_nesting");
tinycc_test!(t67_macro_concat, "67_macro_concat");
tinycc_test!(t70_floating_point_literals, "70_floating_point_literals");
tinycc_test!(t71_macro_empty_arg, "71_macro_empty_arg");
tinycc_test!(t72_long_long_constant, "72_long_long_constant");
tinycc_test!(t75_array_in_struct_init, "75_array_in_struct_init");
tinycc_test!(t76_dollars_in_identifiers, "76_dollars_in_identifiers");
tinycc_test!(t77_push_pop_macro, "77_push_pop_macro");
tinycc_test!(t80_flexarray, "80_flexarray");
tinycc_test!(t81_types, "81_types");
tinycc_test!(t87_dead_code, "87_dead_code");
tinycc_test!(t90_struct_init, "90_struct_init");
tinycc_test!(t92_enum_bitfield, "92_enum_bitfield");
tinycc_test!(t93_integer_promotion, "93_integer_promotion");
tinycc_test!(t97_utf8_string_literal, "97_utf8_string_literal");
tinycc_test!(t100_c99array_decls, "100_c99array_decls");
// t104_inline requires __attribute__((weak)) which toyos-cc doesn't support yet
// tinycc_test!(t104_inline, "104_inline");
tinycc_test!(t105_local_extern, "105_local_extern");
tinycc_test!(t110_average, "110_average");
tinycc_test!(t111_conversion, "111_conversion");
tinycc_test!(t118_switch, "118_switch");
tinycc_test!(t119_random_stuff, "119_random_stuff");
tinycc_test!(t121_struct_return, "121_struct_return");
tinycc_test!(t129_scopes, "129_scopes");
tinycc_test!(t130_large_argument, "130_large_argument");
tinycc_test!(t131_return_struct_in_reg, "131_return_struct_in_reg");
tinycc_test!(t133_old_func, "133_old_func");
tinycc_test!(t134_double_to_signed, "134_double_to_signed");
tinycc_test!(t135_func_arg_struct_compare, "135_func_arg_struct_compare");
tinycc_test!(t137_funcall_struct_args, "137_funcall_struct_args");
tinycc_test!(t138_offsetof, "138_offsetof");
tinycc_test!(t140_switch_hex, "140_switch_hex");
tinycc_test!(t141_tok_str, "141_tok_str");
tinycc_test!(t142_pp_sizeof_ptr, "142_pp_sizeof_ptr");
tinycc_test!(t143_uint64_split, "143_uint64_split");
tinycc_test!(t144_sizeof_init, "144_sizeof_init");
tinycc_test!(t145_self_ref_struct, "145_self_ref_struct");
tinycc_test!(t146_deref_assign, "146_deref_assign");
tinycc_test!(t147_sizeof_deref_array, "147_sizeof_deref_array");
tinycc_test!(t148_directive_in_args, "148_directive_in_args");
tinycc_test!(t149_bitfield_write, "149_bitfield_write");
tinycc_test!(t150_union_short_store, "150_union_short_store");
tinycc_test!(t151_cast_truncate, "151_cast_truncate");
tinycc_test!(t152_float_const_init, "152_float_const_init");
tinycc_test!(t153_sizeof_const_init, "153_sizeof_const_init");
tinycc_test!(t154_funcptr_global_init, "154_funcptr_global_init");
tinycc_test!(t155_addr_array_elem_init, "155_addr_array_elem_init");
tinycc_test!(t156_sizeof_array_count, "156_sizeof_array_count");
tinycc_test!(t157_sizeof_member, "157_sizeof_member");
tinycc_test!(t158_vla, "158_vla");
tinycc_test!(t159_va_list, "159_va_list");
tinycc_test!(t160_global_variadic, "160_global_variadic");
