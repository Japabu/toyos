use toyos_tests::compile::{run_host_test, run_host_test_system_libc};

fn run_test(name: &str, args: &[&str]) {
    run_host_test(name, args, None);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn run_test_x86_64(name: &str, args: &[&str]) {
    run_host_test(name, args, Some("x86_64-apple-darwin"));
}

fn run_test_syslibc(name: &str, args: &[&str]) {
    run_host_test_system_libc(name, args, None);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn run_test_syslibc_x86_64(name: &str, args: &[&str]) {
    run_host_test_system_libc(name, args, Some("x86_64-apple-darwin"));
}

/// Each invocation generates tests for all combinations:
/// - toyos-libc native
/// - toyos-libc x86_64 (macOS ARM64 only, via Rosetta)
/// - system libc native
/// - system libc x86_64 (macOS ARM64 only, via Rosetta)
macro_rules! tinycc_test {
    ($rust_name:ident, $file:expr) => {
        #[test]
        fn $rust_name() {
            run_test($file, &[]);
        }
        mod $rust_name {
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            #[test]
            fn x86_64() {
                super::run_test_x86_64($file, &[]);
            }
            #[test]
            fn syslibc() {
                super::run_test_syslibc($file, &[]);
            }
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            #[test]
            fn syslibc_x86_64() {
                super::run_test_syslibc_x86_64($file, &[]);
            }
        }
    };
    ($rust_name:ident, $file:expr, native_only) => {
        #[test]
        fn $rust_name() {
            run_test($file, &[]);
        }
        mod $rust_name {
            #[test]
            fn syslibc() {
                super::run_test_syslibc($file, &[]);
            }
        }
    };
    ($rust_name:ident, $file:expr, args: [$($arg:expr),*]) => {
        #[test]
        fn $rust_name() {
            run_test($file, &[$($arg),*]);
        }
        mod $rust_name {
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            #[test]
            fn x86_64() {
                super::run_test_x86_64($file, &[$($arg),*]);
            }
            #[test]
            fn syslibc() {
                super::run_test_syslibc($file, &[$($arg),*]);
            }
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            #[test]
            fn syslibc_x86_64() {
                super::run_test_syslibc_x86_64($file, &[$($arg),*]);
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
tinycc_test!(t104_inline, "104_inline");
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
