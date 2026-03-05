use std::fs;
use toyos_tests::compile;
use toyos_tests::qemu;

/// List of C tests to run on ToyOS. Each entry is (test_name, args).
/// Tests that require host-specific features (file I/O paths, Rosetta) are excluded.
const TOYOS_C_TESTS: &[(&str, &[&str])] = &[
    ("00_assignment", &[]),
    ("01_comment", &[]),
    ("02_printf", &[]),
    ("04_for", &[]),
    ("05_array", &[]),
    ("06_case", &[]),
    ("07_function", &[]),
    ("08_while", &[]),
    ("09_do_while", &[]),
    ("10_pointer", &[]),
    ("11_precedence", &[]),
    ("12_hashdefine", &[]),
    ("13_integer_literals", &[]),
    ("14_if", &[]),
    ("15_recursion", &[]),
    ("16_nesting", &[]),
    ("17_enum", &[]),
    ("19_pointer_arithmetic", &[]),
    ("20_pointer_comparison", &[]),
    ("21_char_array", &[]),
    ("22_floating_point", &[]),
    ("23_type_coercion", &[]),
    ("24_math_library", &[]),
    ("25_quicksort", &[]),
    ("26_character_constants", &[]),
    ("27_sizeof", &[]),
    ("28_strings", &[]),
    ("29_array_address", &[]),
    ("30_hanoi", &[]),
    ("33_ternary_op", &[]),
    ("34_array_assignment", &[]),
    ("35_sizeof", &[]),
    ("36_array_initialisers", &[]),
    ("37_sprintf", &[]),
    ("38_multiple_array_index", &[]),
    ("39_typedef", &[]),
    ("41_hashif", &[]),
    ("42_function_pointer", &[]),
    ("43_void_param", &[]),
    ("44_scoped_declarations", &[]),
    ("45_empty_for", &[]),
    ("47_switch_return", &[]),
    ("48_nested_break", &[]),
    ("49_bracket_evaluation", &[]),
    ("50_logical_second_arg", &[]),
    ("51_static", &[]),
    ("52_unnamed_enum", &[]),
    ("54_goto", &[]),
    ("55_lshift_type", &[]),
    ("61_integers", &[]),
    ("64_macro_nesting", &[]),
    ("67_macro_concat", &[]),
    ("70_floating_point_literals", &[]),
    ("71_macro_empty_arg", &[]),
    ("72_long_long_constant", &[]),
    ("75_array_in_struct_init", &[]),
    ("76_dollars_in_identifiers", &[]),
    ("77_push_pop_macro", &[]),
    ("80_flexarray", &[]),
    ("81_types", &[]),
    ("87_dead_code", &[]),
    ("90_struct_init", &[]),
    ("92_enum_bitfield", &[]),
    ("93_integer_promotion", &[]),
    ("97_utf8_string_literal", &[]),
    ("100_c99array_decls", &[]),
    ("104_inline", &[]),
    ("105_local_extern", &[]),
    ("110_average", &[]),
    ("111_conversion", &[]),
    ("118_switch", &[]),
    ("119_random_stuff", &[]),
    ("121_struct_return", &[]),
    ("129_scopes", &[]),
    ("130_large_argument", &[]),
    ("131_return_struct_in_reg", &[]),
    ("133_old_func", &[]),
    ("134_double_to_signed", &[]),
    ("135_func_arg_struct_compare", &[]),
    ("137_funcall_struct_args", &[]),
    ("138_offsetof", &[]),
    ("140_switch_hex", &[]),
    ("141_tok_str", &[]),
    ("142_pp_sizeof_ptr", &[]),
    ("143_uint64_split", &[]),
    ("144_sizeof_init", &[]),
    ("145_self_ref_struct", &[]),
    ("146_deref_assign", &[]),
    ("147_sizeof_deref_array", &[]),
    ("148_directive_in_args", &[]),
    ("149_bitfield_write", &[]),
    ("150_union_short_store", &[]),
    ("151_cast_truncate", &[]),
    ("152_float_const_init", &[]),
    ("153_sizeof_const_init", &[]),
    ("154_funcptr_global_init", &[]),
    ("155_addr_array_elem_init", &[]),
    ("156_sizeof_array_count", &[]),
    ("157_sizeof_member", &[]),
    ("158_vla", &[]),
    ("159_va_list", &[]),
    ("160_global_variadic", &[]),
];

#[test]
#[ignore] // Run with: cargo test --test toyos_c -- --ignored
fn toyos_c_tests() {
    let testcases = compile::testcases_dir();

    // Compile all C tests for x86_64 ELF (ToyOS target)
    // We use x86_64-unknown-linux-gnu as the compile target since ToyOS ELF is Linux-compatible
    let mut c_test_bins: Vec<(String, Vec<u8>)> = Vec::new();

    for (name, _args) in TOYOS_C_TESTS {
        let (obj, extras) = compile::compile_c(name, Some("x86_64-unknown-linux-gnu"));
        let linked = compile::link_toyos(&obj, &extras, name);
        c_test_bins.push((name.to_string(), linked));
    }

    // Run all tests in QEMU
    let session = qemu::run_qemu_tests(&c_test_bins, &[]);

    // Verify results
    let mut failures = Vec::new();

    let total = session.results.len();
    let mut passed = 0usize;

    for result in &session.results {
        let test_name = result.name.strip_prefix("test_c_").unwrap_or(&result.name);

        if let Some(err) = &result.error {
            eprintln!("FAIL {test_name}: error: {err}");
            failures.push(format!("{test_name}: error: {err}"));
            continue;
        }

        match result.exit_code {
            Some(0) => {
                // Check output against .expect file
                let expect_file = testcases.join(format!("{test_name}.expect"));
                if expect_file.exists() {
                    let expected = fs::read_to_string(&expect_file).unwrap();
                    let actual = result.stdout.trim_end();
                    let expected = expected.trim_end();
                    if actual != expected {
                        eprintln!("FAIL {test_name}: output mismatch");
                        failures.push(format!(
                            "{test_name}: output mismatch\n--- expected ---\n{expected}\n--- actual ---\n{actual}"
                        ));
                    } else {
                        eprintln!("PASS {test_name}");
                        passed += 1;
                    }
                } else {
                    eprintln!("PASS {test_name} (no .expect file)");
                    passed += 1;
                }
            }
            Some(code) => {
                eprintln!("FAIL {test_name}: exited with code {code}");
                failures.push(format!("{test_name}: exited with code {code}\nstdout: {}", result.stdout));
            }
            None => {
                eprintln!("FAIL {test_name}: no exit code");
                failures.push(format!("{test_name}: no exit code"));
            }
        }
    }

    eprintln!("\n{passed}/{total} tests passed");

    if !failures.is_empty() {
        panic!(
            "{} test(s) failed:\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }

    eprintln!("All {} ToyOS C tests passed!", session.results.len());
}

#[test]
#[ignore]
fn toyos_c_single() {
    let name = std::env::var("TOYOS_TEST_NAME").unwrap_or_else(|_| "22_floating_point".to_string());
    let testcases = compile::testcases_dir();

    let (obj, extras) = compile::compile_c(&name, Some("x86_64-unknown-linux-gnu"));
    let linked = compile::link_toyos(&obj, &extras, &name);
    let c_test_bins = vec![(name.clone(), linked)];

    let session = qemu::run_qemu_tests(&c_test_bins, &[]);

    for result in &session.results {
        let test_name = result.name.strip_prefix("test_c_").unwrap_or(&result.name);
        if let Some(err) = &result.error {
            panic!("FAIL {test_name}: {err}");
        }
        match result.exit_code {
            Some(0) => {
                let expect_file = testcases.join(format!("{test_name}.expect"));
                if expect_file.exists() {
                    let expected = fs::read_to_string(&expect_file).unwrap();
                    assert_eq!(result.stdout.trim_end(), expected.trim_end(),
                        "output mismatch for {test_name}");
                }
                eprintln!("PASS {test_name}");
            }
            Some(code) => panic!("FAIL {test_name}: exit code {code}\nstdout: {}", result.stdout),
            None => panic!("FAIL {test_name}: no exit code"),
        }
    }
}
