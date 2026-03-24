use std::collections::HashMap;
use std::sync::LazyLock;
mod common;
use common::compile;

type CompiledObjs = (Vec<u8>, Vec<Vec<u8>>);

fn compile_all(target: Option<&str>) -> HashMap<&'static str, CompiledObjs> {
    let label = target.unwrap_or("native");
    eprintln!("[host_c] Compiling {} C tests for {label}...", TESTS.len());
    let mut cache = HashMap::new();
    for &name in TESTS {
        cache.insert(name, compile::compile_c(name, target));
    }
    eprintln!("[host_c] Done compiling for {label}.");
    cache
}

static NATIVE_CACHE: LazyLock<HashMap<&str, CompiledObjs>> =
    LazyLock::new(|| compile_all(None));

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
static X86_64_CACHE: LazyLock<HashMap<&str, CompiledObjs>> =
    LazyLock::new(|| compile_all(Some("x86_64-apple-darwin")));

fn run_test(name: &str, args: &[&str]) {
    let (obj, extras) = NATIVE_CACHE.get(name).unwrap_or_else(|| panic!("test {name} not in cache"));
    compile::run_host_test(obj, extras, name, args, None);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn run_test_x86_64(name: &str, args: &[&str]) {
    let (obj, extras) = X86_64_CACHE.get(name).unwrap_or_else(|| panic!("test {name} not in cache"));
    compile::run_host_test(obj, extras, name, args, Some("x86_64-apple-darwin"));
}

/// Macro that generates TESTS list and test functions.
///
/// Each entry is one of:
///   ($func, $name)                          — standard test (all variants)
///   ($func, $name, native_only)             — skip x86_64 variant
///   ($func, $name, args: [..])              — test with arguments
macro_rules! host_c_tests {
    ($( ($func:ident, $name:expr $(, $($modifier:tt)* )? ) ),* $(,)?) => {
        const TESTS: &[&str] = &[$($name),*];

        $( host_c_tests!(@test $func, $name $(, $($modifier)* )? ); )*
    };

    // Standard test
    (@test $func:ident, $file:expr) => {
        #[test]
        fn $func() {
            run_test($file, &[]);
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        mod $func {
            #[test]
            fn x86_64() {
                super::run_test_x86_64($file, &[]);
            }
        }
    };

    // Native-only test: skip x86_64 variant
    (@test $func:ident, $file:expr, native_only) => {
        #[test]
        fn $func() {
            run_test($file, &[]);
        }
    };

    // Test with arguments
    (@test $func:ident, $file:expr, args: [$($arg:expr),*]) => {
        #[test]
        fn $func() {
            run_test($file, &[$($arg),*]);
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        mod $func {
            #[test]
            fn x86_64() {
                super::run_test_x86_64($file, &[$($arg),*]);
            }
        }
    };
}

host_c_tests!(
    (t00_assignment, "00_assignment"),
    (t01_comment, "01_comment"),
    (t02_printf, "02_printf"),
    (t04_for, "04_for"),
    (t05_array, "05_array"),
    (t06_case, "06_case"),
    (t07_function, "07_function"),
    (t08_while, "08_while"),
    (t09_do_while, "09_do_while"),
    (t10_pointer, "10_pointer"),
    (t11_precedence, "11_precedence"),
    (t12_hashdefine, "12_hashdefine"),
    (t13_integer_literals, "13_integer_literals"),
    (t14_if, "14_if"),
    (t15_recursion, "15_recursion"),
    (t16_nesting, "16_nesting"),
    (t17_enum, "17_enum"),
    (t18_include, "18_include"),
    (t19_pointer_arithmetic, "19_pointer_arithmetic"),
    (t20_pointer_comparison, "20_pointer_comparison"),
    (t21_char_array, "21_char_array"),
    // x86_64 `long double` is 80-bit; our compiler uses 64-bit double — skip x86_64.
    (t22_floating_point, "22_floating_point", native_only),
    (t23_type_coercion, "23_type_coercion"),
    (t24_math_library, "24_math_library"),
    (t25_quicksort, "25_quicksort"),
    (t26_character_constants, "26_character_constants"),
    (t27_sizeof, "27_sizeof"),
    (t28_strings, "28_strings"),
    (t29_array_address, "29_array_address"),
    (t30_hanoi, "30_hanoi"),
    (t31_args, "31_args", args: ["arg1", "arg2", "arg3", "arg4", "arg5"]),
    (t32_led, "32_led", args: ["12345"]),
    // (t33_ternary_op, "33_ternary_op"), // needs _Generic
    (t34_array_assignment, "34_array_assignment"),
    (t35_sizeof, "35_sizeof"),
    (t36_array_initialisers, "36_array_initialisers"),
    (t37_sprintf, "37_sprintf"),
    (t38_multiple_array_index, "38_multiple_array_index"),
    (t39_typedef, "39_typedef"),
    (t40_stdio, "40_stdio"),
    (t41_hashif, "41_hashif"),
    (t42_function_pointer, "42_function_pointer"),
    (t43_void_param, "43_void_param"),
    (t44_scoped_declarations, "44_scoped_declarations"),
    (t45_empty_for, "45_empty_for"),
    (t46_grep, "46_grep", args: ["[^* ]*[:a:d: ]+\\:\\*-/: $", "46_grep.c"]),
    (t47_switch_return, "47_switch_return"),
    (t48_nested_break, "48_nested_break"),
    (t49_bracket_evaluation, "49_bracket_evaluation"),
    (t50_logical_second_arg, "50_logical_second_arg"),
    (t51_static, "51_static"),
    (t52_unnamed_enum, "52_unnamed_enum"),
    (t54_goto, "54_goto"),
    (t55_lshift_type, "55_lshift_type"),
    (t61_integers, "61_integers"),
    (t64_macro_nesting, "64_macro_nesting"),
    (t67_macro_concat, "67_macro_concat"),
    (t70_floating_point_literals, "70_floating_point_literals"),
    (t71_macro_empty_arg, "71_macro_empty_arg"),
    (t72_long_long_constant, "72_long_long_constant"),
    (t75_array_in_struct_init, "75_array_in_struct_init"),
    (t76_dollars_in_identifiers, "76_dollars_in_identifiers"),
    (t77_push_pop_macro, "77_push_pop_macro"),
    (t80_flexarray, "80_flexarray"),
    (t81_types, "81_types"),
    (t87_dead_code, "87_dead_code"),
    (t90_struct_init, "90_struct_init"),
    (t92_enum_bitfield, "92_enum_bitfield"),
    (t93_integer_promotion, "93_integer_promotion"),
    (t97_utf8_string_literal, "97_utf8_string_literal"),
    (t100_c99array_decls, "100_c99array_decls"),
    // (t104_inline, "104_inline"), // needs weak symbols in linker
    (t105_local_extern, "105_local_extern"),
    (t110_average, "110_average"),
    (t111_conversion, "111_conversion"),
    (t118_switch, "118_switch"),
    (t119_random_stuff, "119_random_stuff"),
    (t121_struct_return, "121_struct_return"),
    (t129_scopes, "129_scopes"),
    (t130_large_argument, "130_large_argument"),
    (t131_return_struct_in_reg, "131_return_struct_in_reg"),
    (t133_old_func, "133_old_func"),
    (t134_double_to_signed, "134_double_to_signed"),
    (t135_func_arg_struct_compare, "135_func_arg_struct_compare"),
    (t137_funcall_struct_args, "137_funcall_struct_args"),
    (t138_offsetof, "138_offsetof"),
    (t140_switch_hex, "140_switch_hex"),
    (t141_tok_str, "141_tok_str"),
    (t142_pp_sizeof_ptr, "142_pp_sizeof_ptr"),
    (t143_uint64_split, "143_uint64_split"),
    (t144_sizeof_init, "144_sizeof_init"),
    (t145_self_ref_struct, "145_self_ref_struct"),
    (t146_deref_assign, "146_deref_assign"),
    (t147_sizeof_deref_array, "147_sizeof_deref_array"),
    (t148_directive_in_args, "148_directive_in_args"),
    (t149_bitfield_write, "149_bitfield_write"),
    (t150_union_short_store, "150_union_short_store"),
    (t151_cast_truncate, "151_cast_truncate"),
    (t152_float_const_init, "152_float_const_init"),
    (t153_sizeof_const_init, "153_sizeof_const_init"),
    (t154_funcptr_global_init, "154_funcptr_global_init"),
    (t155_addr_array_elem_init, "155_addr_array_elem_init"),
    (t156_sizeof_array_count, "156_sizeof_array_count"),
    (t157_sizeof_member, "157_sizeof_member"),
    (t158_vla, "158_vla"),
    (t159_va_list, "159_va_list"),
    (t160_global_variadic, "160_global_variadic"),
    (t200_variadic_float, "200_variadic_float"),
);
