use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::Duration;
use toyos_tests::compile;
use toyos_tests::qemu::{QemuInstance, TestResult};

const TOYOS_C_TESTS: &[&str] = &[
    "00_assignment",
    "01_comment",
    "02_printf",
    "04_for",
    "05_array",
    "06_case",
    "07_function",
    "08_while",
    "09_do_while",
    "10_pointer",
    "11_precedence",
    "12_hashdefine",
    "13_integer_literals",
    "14_if",
    "15_recursion",
    "16_nesting",
    "17_enum",
    "19_pointer_arithmetic",
    "20_pointer_comparison",
    "21_char_array",
    "22_floating_point",
    "23_type_coercion",
    "24_math_library",
    "25_quicksort",
    "26_character_constants",
    "27_sizeof",
    "28_strings",
    "29_array_address",
    "30_hanoi",
    // "33_ternary_op", // needs _Generic

    "34_array_assignment",
    "35_sizeof",
    "36_array_initialisers",
    "37_sprintf",
    "38_multiple_array_index",
    "39_typedef",
    "41_hashif",
    "42_function_pointer",
    "43_void_param",
    "44_scoped_declarations",
    "45_empty_for",
    "47_switch_return",
    "48_nested_break",
    "49_bracket_evaluation",
    "50_logical_second_arg",
    "51_static",
    "52_unnamed_enum",
    "54_goto",
    "55_lshift_type",
    "61_integers",
    "64_macro_nesting",
    "67_macro_concat",
    "70_floating_point_literals",
    "71_macro_empty_arg",
    "72_long_long_constant",
    "75_array_in_struct_init",
    "76_dollars_in_identifiers",
    "77_push_pop_macro",
    "80_flexarray",
    "81_types",
    "87_dead_code",
    "90_struct_init",
    "92_enum_bitfield",
    "93_integer_promotion",
    "97_utf8_string_literal",
    "100_c99array_decls",
    // "104_inline", // needs weak symbols in linker

    "105_local_extern",
    "110_average",
    "111_conversion",
    "118_switch",
    "119_random_stuff",
    "121_struct_return",
    "129_scopes",
    "130_large_argument",
    "131_return_struct_in_reg",
    "133_old_func",
    "134_double_to_signed",
    "135_func_arg_struct_compare",
    "137_funcall_struct_args",
    "138_offsetof",
    "140_switch_hex",
    "141_tok_str",
    "142_pp_sizeof_ptr",
    "143_uint64_split",
    "144_sizeof_init",
    "145_self_ref_struct",
    "146_deref_assign",
    "147_sizeof_deref_array",
    "148_directive_in_args",
    "149_bitfield_write",
    "150_union_short_store",
    "151_cast_truncate",
    "152_float_const_init",
    "153_sizeof_const_init",
    "154_funcptr_global_init",
    "155_addr_array_elem_init",
    "156_sizeof_array_count",
    "157_sizeof_member",
    "158_vla",
    "159_va_list",
    "160_global_variadic",
];

static QEMU: LazyLock<Mutex<QemuInstance>> = LazyLock::new(|| {
    eprintln!("[toyos_c] Compiling {} C tests for ToyOS...", TOYOS_C_TESTS.len());
    let mut c_test_bins: Vec<(String, Vec<u8>)> = Vec::new();
    for name in TOYOS_C_TESTS {
        let result = std::panic::catch_unwind(|| {
            let (obj, extras) = compile::compile_c(name, Some("x86_64-unknown-toyos"));
            compile::link_toyos(&obj, &extras, name)
        });
        match result {
            Ok(linked) => c_test_bins.push((name.to_string(), linked)),
            Err(_) => eprintln!("[toyos_c] SKIP {name}: compilation panicked"),
        }
    }
    eprintln!("[toyos_c] Booting QEMU with {} test binaries...", c_test_bins.len());
    Mutex::new(QemuInstance::boot(&c_test_bins, &[]))
});

fn check_test_result(result: &TestResult) {
    let test_name = result.name.strip_prefix("test_c_").unwrap_or(&result.name);

    if let Some(err) = &result.error {
        if err.contains("No such file") || err.contains("not found") {
            panic!("SKIP {test_name}: not compiled (likely unsupported feature)");
        }
        panic!("FAIL {test_name}: {err}");
    }

    match result.exit_code {
        Some(0) => {
            let testcases = compile::testcases_dir();
            let expect_file = testcases.join(format!("{test_name}.expect"));
            if expect_file.exists() {
                let expected = fs::read_to_string(&expect_file).unwrap();
                assert_eq!(
                    result.stdout.trim_end(),
                    expected.trim_end(),
                    "output mismatch for {test_name}\n--- expected ---\n{}\n--- actual ---\n{}",
                    expected.trim_end(),
                    result.stdout.trim_end(),
                );
            }
        }
        Some(code) => panic!("FAIL {test_name}: exit code {code}\nstdout: {}", result.stdout),
        None => panic!("FAIL {test_name}: no exit code"),
    }
}

macro_rules! toyos_c_test {
    ($func_name:ident, $test_name:expr) => {
        #[test]
        fn $func_name() {
            let result = {
                let mut qemu = QEMU.lock().unwrap_or_else(|e| e.into_inner());
                qemu.run_test(
                    &format!("test_c_{}", $test_name),
                    Duration::from_secs(10),
                )
            };
            check_test_result(&result);
        }
    };
}

toyos_c_test!(t00_assignment, "00_assignment");
toyos_c_test!(t01_comment, "01_comment");
toyos_c_test!(t02_printf, "02_printf");
toyos_c_test!(t04_for, "04_for");
toyos_c_test!(t05_array, "05_array");
toyos_c_test!(t06_case, "06_case");
toyos_c_test!(t07_function, "07_function");
toyos_c_test!(t08_while, "08_while");
toyos_c_test!(t09_do_while, "09_do_while");
toyos_c_test!(t10_pointer, "10_pointer");
toyos_c_test!(t11_precedence, "11_precedence");
toyos_c_test!(t12_hashdefine, "12_hashdefine");
toyos_c_test!(t13_integer_literals, "13_integer_literals");
toyos_c_test!(t14_if, "14_if");
toyos_c_test!(t15_recursion, "15_recursion");
toyos_c_test!(t16_nesting, "16_nesting");
toyos_c_test!(t17_enum, "17_enum");
toyos_c_test!(t19_pointer_arithmetic, "19_pointer_arithmetic");
toyos_c_test!(t20_pointer_comparison, "20_pointer_comparison");
toyos_c_test!(t21_char_array, "21_char_array");
toyos_c_test!(t22_floating_point, "22_floating_point");
toyos_c_test!(t23_type_coercion, "23_type_coercion");
toyos_c_test!(t24_math_library, "24_math_library");
toyos_c_test!(t25_quicksort, "25_quicksort");
toyos_c_test!(t26_character_constants, "26_character_constants");
toyos_c_test!(t27_sizeof, "27_sizeof");
toyos_c_test!(t28_strings, "28_strings");
toyos_c_test!(t29_array_address, "29_array_address");
toyos_c_test!(t30_hanoi, "30_hanoi");
// toyos_c_test!(t33_ternary_op, "33_ternary_op"); // needs _Generic
toyos_c_test!(t34_array_assignment, "34_array_assignment");
toyos_c_test!(t35_sizeof, "35_sizeof");
toyos_c_test!(t36_array_initialisers, "36_array_initialisers");
toyos_c_test!(t37_sprintf, "37_sprintf");
toyos_c_test!(t38_multiple_array_index, "38_multiple_array_index");
toyos_c_test!(t39_typedef, "39_typedef");
toyos_c_test!(t41_hashif, "41_hashif");
toyos_c_test!(t42_function_pointer, "42_function_pointer");
toyos_c_test!(t43_void_param, "43_void_param");
toyos_c_test!(t44_scoped_declarations, "44_scoped_declarations");
toyos_c_test!(t45_empty_for, "45_empty_for");
toyos_c_test!(t47_switch_return, "47_switch_return");
toyos_c_test!(t48_nested_break, "48_nested_break");
toyos_c_test!(t49_bracket_evaluation, "49_bracket_evaluation");
toyos_c_test!(t50_logical_second_arg, "50_logical_second_arg");
toyos_c_test!(t51_static, "51_static");
toyos_c_test!(t52_unnamed_enum, "52_unnamed_enum");
toyos_c_test!(t54_goto, "54_goto");
toyos_c_test!(t55_lshift_type, "55_lshift_type");
toyos_c_test!(t61_integers, "61_integers");
toyos_c_test!(t64_macro_nesting, "64_macro_nesting");
toyos_c_test!(t67_macro_concat, "67_macro_concat");
toyos_c_test!(t70_floating_point_literals, "70_floating_point_literals");
toyos_c_test!(t71_macro_empty_arg, "71_macro_empty_arg");
toyos_c_test!(t72_long_long_constant, "72_long_long_constant");
toyos_c_test!(t75_array_in_struct_init, "75_array_in_struct_init");
toyos_c_test!(t76_dollars_in_identifiers, "76_dollars_in_identifiers");
toyos_c_test!(t77_push_pop_macro, "77_push_pop_macro");
toyos_c_test!(t80_flexarray, "80_flexarray");
toyos_c_test!(t81_types, "81_types");
toyos_c_test!(t87_dead_code, "87_dead_code");
toyos_c_test!(t90_struct_init, "90_struct_init");
toyos_c_test!(t92_enum_bitfield, "92_enum_bitfield");
toyos_c_test!(t93_integer_promotion, "93_integer_promotion");
toyos_c_test!(t97_utf8_string_literal, "97_utf8_string_literal");
toyos_c_test!(t100_c99array_decls, "100_c99array_decls");
// toyos_c_test!(t104_inline, "104_inline"); // needs weak symbols in linker
toyos_c_test!(t105_local_extern, "105_local_extern");
toyos_c_test!(t110_average, "110_average");
toyos_c_test!(t111_conversion, "111_conversion");
toyos_c_test!(t118_switch, "118_switch");
toyos_c_test!(t119_random_stuff, "119_random_stuff");
toyos_c_test!(t121_struct_return, "121_struct_return");
toyos_c_test!(t129_scopes, "129_scopes");
toyos_c_test!(t130_large_argument, "130_large_argument");
toyos_c_test!(t131_return_struct_in_reg, "131_return_struct_in_reg");
toyos_c_test!(t133_old_func, "133_old_func");
toyos_c_test!(t134_double_to_signed, "134_double_to_signed");
toyos_c_test!(t135_func_arg_struct_compare, "135_func_arg_struct_compare");
toyos_c_test!(t137_funcall_struct_args, "137_funcall_struct_args");
toyos_c_test!(t138_offsetof, "138_offsetof");
toyos_c_test!(t140_switch_hex, "140_switch_hex");
toyos_c_test!(t141_tok_str, "141_tok_str");
toyos_c_test!(t142_pp_sizeof_ptr, "142_pp_sizeof_ptr");
toyos_c_test!(t143_uint64_split, "143_uint64_split");
toyos_c_test!(t144_sizeof_init, "144_sizeof_init");
toyos_c_test!(t145_self_ref_struct, "145_self_ref_struct");
toyos_c_test!(t146_deref_assign, "146_deref_assign");
toyos_c_test!(t147_sizeof_deref_array, "147_sizeof_deref_array");
toyos_c_test!(t148_directive_in_args, "148_directive_in_args");
toyos_c_test!(t149_bitfield_write, "149_bitfield_write");
toyos_c_test!(t150_union_short_store, "150_union_short_store");
toyos_c_test!(t151_cast_truncate, "151_cast_truncate");
toyos_c_test!(t152_float_const_init, "152_float_const_init");
toyos_c_test!(t153_sizeof_const_init, "153_sizeof_const_init");
toyos_c_test!(t154_funcptr_global_init, "154_funcptr_global_init");
toyos_c_test!(t155_addr_array_elem_init, "155_addr_array_elem_init");
toyos_c_test!(t156_sizeof_array_count, "156_sizeof_array_count");
toyos_c_test!(t157_sizeof_member, "157_sizeof_member");
toyos_c_test!(t158_vla, "158_vla");
toyos_c_test!(t159_va_list, "159_va_list");
toyos_c_test!(t160_global_variadic, "160_global_variadic");

/// Debug mode: boots QEMU with GDB stub, then polls for commands via file IPC.
///
/// Usage:
///   cargo test --test toyos_c -- --ignored debug --nocapture
///
/// Once running, send serial commands from any shell:
///   echo "run test_c_49_bracket_evaluation" > /tmp/toyos-debug-cmd
///   cat /tmp/toyos-debug-result    # read the result
///   echo "quit" > /tmp/toyos-debug-cmd  # shut down
///
/// The test also writes all serial output to /tmp/toyos-debug-serial.log.
/// GDB stub is on localhost:1234 for LLDB/GDB attachment.
#[test]
#[ignore]
fn debug() {
    let cmd_path = Path::new("/tmp/toyos-debug-cmd");
    let result_path = Path::new("/tmp/toyos-debug-result");
    let ready_path = Path::new("/tmp/toyos-debug-ready");

    // Clean up stale files
    let _ = fs::remove_file(cmd_path);
    let _ = fs::remove_file(result_path);
    let _ = fs::remove_file(ready_path);

    eprintln!("[debug] Compiling C tests for ToyOS...");
    let mut c_test_bins: Vec<(String, Vec<u8>)> = Vec::new();
    for name in TOYOS_C_TESTS {
        let result = std::panic::catch_unwind(|| {
            let (obj, extras) = compile::compile_c(name, Some("x86_64-unknown-toyos"));
            compile::link_toyos(&obj, &extras, name)
        });
        match result {
            Ok(linked) => c_test_bins.push((name.to_string(), linked)),
            Err(_) => eprintln!("[debug] SKIP {name}: compilation panicked"),
        }
    }

    eprintln!("[debug] Booting QEMU in debug mode with {} test binaries...", c_test_bins.len());
    let mut qemu = QemuInstance::boot_with_options(&c_test_bins, &[], true);

    let repo = compile::repo_root();
    let kernel_elf = repo.join("kernel/target/x86_64-unknown-none/debug/kernel");

    eprintln!();
    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║  QEMU running with GDB stub on localhost:1234               ║");
    eprintln!("╠══════════════════════════════════════════════════════════════╣");
    eprintln!("║  Kernel ELF: {}", kernel_elf.display());
    eprintln!("║                                                              ║");
    eprintln!("║  Send commands:                                              ║");
    eprintln!("║    echo 'run test_c_49_bracket_evaluation' > {}    ║", cmd_path.display());
    eprintln!("║    cat {}                                 ║", result_path.display());
    eprintln!("║    echo 'quit' > {}                                ║", cmd_path.display());
    eprintln!("╚══════════════════════════════════════════════════════════════╝");

    // Signal that we're ready
    fs::write(ready_path, "ready\n").unwrap();

    // Poll for commands
    loop {
        thread::sleep(Duration::from_millis(200));

        let cmd = match fs::read_to_string(cmd_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let _ = fs::remove_file(cmd_path);
        let cmd = cmd.trim();
        if cmd.is_empty() { continue; }

        if cmd == "quit" || cmd == "q" {
            eprintln!("[debug] Quit requested");
            let _ = fs::write(result_path, "quit\n");
            break;
        }

        if let Some(test_name) = cmd.strip_prefix("run ") {
            let test_name = test_name.trim();
            eprintln!("[debug] Running {test_name}...");
            let result = qemu.run_test(test_name, Duration::from_secs(60));

            let mut output = String::new();
            output.push_str(&format!("test: {}\n", result.name));
            output.push_str(&format!("exit_code: {:?}\n", result.exit_code));
            if let Some(err) = &result.error {
                output.push_str(&format!("error: {err}\n"));
            }
            if !result.stdout.is_empty() {
                output.push_str("--- stdout ---\n");
                output.push_str(&result.stdout);
            }
            eprintln!("[debug] {output}");
            fs::write(result_path, &output).unwrap();
        } else {
            // Raw serial command: write directly to QEMU stdin
            eprintln!("[debug] Sending raw serial: {cmd}");
            use std::io::Write;
            writeln!(qemu.stdin_mut(), "{cmd}").expect("Failed to write to QEMU stdin");
            qemu.flush_stdin();
            fs::write(result_path, "sent\n").unwrap();
        }
    }

    let _ = fs::remove_file(ready_path);
    eprintln!("[debug] Shutting down QEMU...");
}
