use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::Duration;
mod common;
use common::compile;
use common::qemu::{QemuInstance, TestResult};

macro_rules! toyos_c_tests {
    ($(($func:ident, $name:expr)),* $(,)?) => {
        const TOYOS_C_TESTS: &[&str] = &[$($name),*];

        $(
            #[test]
            fn $func() {
                let result = {
                    let mut qemu = QEMU.lock().unwrap_or_else(|e| e.into_inner());
                    qemu.run_test(
                        &format!("test_c_{}", $name),
                        Duration::from_secs(10),
                    )
                };
                check_test_result(&result);
            }
        )*
    };
}

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

toyos_c_tests!(
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
    (t19_pointer_arithmetic, "19_pointer_arithmetic"),
    (t20_pointer_comparison, "20_pointer_comparison"),
    (t21_char_array, "21_char_array"),
    (t22_floating_point, "22_floating_point"),
    (t23_type_coercion, "23_type_coercion"),
    (t24_math_library, "24_math_library"),
    (t25_quicksort, "25_quicksort"),
    (t26_character_constants, "26_character_constants"),
    (t27_sizeof, "27_sizeof"),
    (t28_strings, "28_strings"),
    (t29_array_address, "29_array_address"),
    (t30_hanoi, "30_hanoi"),
    // (t33_ternary_op, "33_ternary_op"), // needs _Generic
    (t34_array_assignment, "34_array_assignment"),
    (t35_sizeof, "35_sizeof"),
    (t36_array_initialisers, "36_array_initialisers"),
    (t37_sprintf, "37_sprintf"),
    (t38_multiple_array_index, "38_multiple_array_index"),
    (t39_typedef, "39_typedef"),
    (t41_hashif, "41_hashif"),
    (t42_function_pointer, "42_function_pointer"),
    (t43_void_param, "43_void_param"),
    (t44_scoped_declarations, "44_scoped_declarations"),
    (t45_empty_for, "45_empty_for"),
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
    (t90_static_vs_global, "90_static_vs_global"),
);

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
    let mut qemu = QemuInstance::boot_with_options(&c_test_bins, &[], true, true);

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
