use cranelift_codegen::ir::{types, AbiParam, Function, InstBuilder, Signature, UserFuncName};
use cranelift_codegen::isa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use target_lexicon::triple;

/// Compile a trivial function using cranelift and return 0 on success.
/// This exercises the exact same VCode::emit → buffer.finish() path
/// that triggers the timing assertion.
#[no_mangle]
pub extern "C" fn cl_compile_trivial() -> u64 {
    let mut flag_builder = settings::builder();
    flag_builder.set("opt_level", "speed").unwrap();
    let flags = settings::Flags::new(flag_builder);

    let isa = match isa::lookup(triple!("x86_64-unknown-unknown")) {
        Ok(b) => b.finish(flags).unwrap(),
        Err(_) => return 100,
    };

    let mut sig = Signature::new(isa.default_call_conv());
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));

    let mut func = Function::with_name_signature(UserFuncName::testcase("test"), sig);
    let mut builder_ctx = FunctionBuilderContext::new();

    {
        let mut builder = FunctionBuilder::new(&mut func, &mut builder_ctx);
        let block = builder.create_block();
        builder.append_block_params_for_function_params(block);
        builder.switch_to_block(block);
        builder.seal_block(block);

        let param = builder.block_params(block)[0];
        let one = builder.ins().iconst(types::I64, 1);
        let result = builder.ins().iadd(param, one);
        builder.ins().return_(&[result]);
        builder.finalize();
    }

    let mut ctx = Context::for_function(func);
    match ctx.compile(isa.as_ref(), &mut Default::default()) {
        Ok(_) => 0,
        Err(_) => 1,
    }
}

/// Test that std::thread::spawn works from inside a cdylib .so
#[no_mangle]
pub extern "C" fn cl_thread_test() -> u64 {
    println!("[cl_thread_test] spawning thread from .so...");
    let handle = std::thread::spawn(|| {
        println!("[cl_thread_test] thread running!");
        42u64
    });
    println!("[cl_thread_test] waiting for join...");
    match handle.join() {
        Ok(v) => { println!("[cl_thread_test] joined: {}", v); v }
        Err(_) => { println!("[cl_thread_test] join failed!"); 0 }
    }
}

/// Simulates the rustc pattern: compile on main thread with DefaultProfiler,
/// then spawn a thread that installs a custom profiler and compiles again.
#[no_mangle]
pub extern "C" fn cl_compile_with_profiler_swap() -> u64 {
    // First compile on main thread (uses DefaultProfiler)
    let result = cl_compile_trivial();
    if result != 0 { return 200 + result; }

    println!("[cl] main compile ok, spawning thread...");
    // Spawn thread, install custom profiler, compile again
    let handle = std::thread::spawn(|| {
        println!("[cl] thread started, setting profiler...");
        struct NoopProfiler;
        impl cranelift_codegen::timing::Profiler for NoopProfiler {
            fn start_pass(&self, _pass: cranelift_codegen::timing::Pass) -> Box<dyn std::any::Any> {
                Box::new(())
            }
        }
        cranelift_codegen::timing::set_thread_profiler(Box::new(NoopProfiler));
        println!("[cl] profiler set, compiling...");
        let r = cl_compile_trivial();
        println!("[cl] thread compile done: {}", r);
        r
    });
    match handle.join() {
        Ok(0) => 0,
        Ok(n) => 300 + n,
        Err(_) => 400,
    }
}
