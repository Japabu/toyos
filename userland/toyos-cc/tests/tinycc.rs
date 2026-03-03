use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

fn toyos_cc() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_toyos-cc"))
}

fn testcases_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testcases/tinycc")
}

fn read_object(path: &Path) -> (String, Vec<u8>) {
    let data = fs::read(path).unwrap_or_else(|e| {
        panic!("cannot read {}: {e}", path.display());
    });
    (path.display().to_string(), data)
}

fn system_include_args() -> Vec<String> {
    if cfg!(target_os = "macos") {
        let output = Command::new("xcrun")
            .args(["--show-sdk-path"])
            .output()
            .expect("failed to run xcrun");
        let sdk = String::from_utf8(output.stdout).unwrap().trim().to_string();
        vec!["-I".to_string(), format!("{sdk}/usr/include")]
    } else {
        vec!["-I".to_string(), "/usr/include".to_string()]
    }
}

fn run_test(name: &str, args: &[&str]) {
    let dir = testcases_dir();
    let c_file = dir.join(format!("{name}.c"));
    let expect_file = dir.join(format!("{name}.expect"));

    assert!(c_file.exists(), "missing test file: {}", c_file.display());
    assert!(
        expect_file.exists(),
        "missing expect file: {}",
        expect_file.display()
    );

    let expected = fs::read_to_string(&expect_file).unwrap();

    let tmp = env::temp_dir().join(format!("toyos-cc-test-{name}"));
    let obj = tmp.with_extension("o");
    let bin = tmp.with_extension("bin");

    // Compile (run from testcases dir so __FILE__ uses relative path)
    let compile = Command::new(toyos_cc())
        .current_dir(&dir)
        .args(["-c", "-o"])
        .arg(&obj)
        .args(system_include_args())
        .arg("-I")
        .arg(&dir)
        .arg(format!("{name}.c"))
        .output()
        .expect("failed to run toyos-cc");

    assert!(
        compile.status.success(),
        "toyos-cc failed to compile {name}.c:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );

    // Compile companion files (e.g., "104+_inline.c" for "104_inline")
    let mut extra_objs = Vec::new();
    if let Some(idx) = name.find('_') {
        let prefix = &name[..idx];
        let suffix = &name[idx..]; // e.g. "_inline"
        let companion_name = format!("{}+{}.c", prefix, suffix);
        let companion = dir.join(&companion_name);
        if companion.exists() {
            let extra_obj = tmp.with_extension("extra.o");
            let cc_compile = Command::new(toyos_cc())
                .current_dir(&dir)
                .args(["-c", "-o"])
                .arg(&extra_obj)
                .args(system_include_args())
                .arg("-I")
                .arg(&dir)
                .arg(&companion)
                .output()
                .expect("failed to compile companion file");
            assert!(
                cc_compile.status.success(),
                "toyos-cc failed to compile companion {companion_name}:\nstderr: {}",
                String::from_utf8_lossy(&cc_compile.stderr),
            );
            extra_objs.push(extra_obj);
        }
    }

    // Link with toyos-ld
    let mut objects = vec![read_object(&obj)];
    for extra in &extra_objs {
        objects.push(read_object(extra));
    }

    let _ = fs::remove_file(&obj);
    for extra in &extra_objs {
        let _ = fs::remove_file(extra);
    }

    let entry = if cfg!(target_os = "macos") { "_main" } else { "main" };
    let linked = if cfg!(target_os = "macos") {
        toyos_ld::link_macho(&objects, entry, false)
    } else {
        toyos_ld::link_full(&objects, entry, false, false)
    };
    let linked = linked.unwrap_or_else(|e| panic!("toyos-ld failed for {name}: {e}"));
    fs::write(&bin, &linked).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Run
    let run = Command::new(&bin)
        .args(args)
        .current_dir(&dir)
        .output()
        .expect("failed to run test binary");

    let _ = fs::remove_file(&bin);

    let actual = String::from_utf8_lossy(&run.stdout);

    assert_eq!(
        actual.trim_end(),
        expected.trim_end(),
        "output mismatch for {name}\n--- expected ---\n{}\n--- actual ---\n{}",
        expected.trim_end(),
        actual.trim_end(),
    );
}

macro_rules! tinycc_test {
    ($rust_name:ident, $file:expr) => {
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
tinycc_test!(t22_floating_point, "22_floating_point");
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
tinycc_test!(t33_ternary_op, "33_ternary_op");
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
