//#![no_std]
#![feature(lang_items)]
#![feature(asm)]
#![feature(naked_functions)]
#![feature(thread_local)]
//#![no_main]

/*
#[no_mangle]
pub extern "C" fn std_runtime_starta() {
    twizzler_abi::syscall::sys_kernel_console_write(
        b"hello world\n",
        twizzler_abi::syscall::KernelConsoleWriteFlags::empty(),
    );
    loop {}
}
*/

/*
#[panic_handler]
pub fn __panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
*/

#[thread_local]
static mut FOO: u32 = 42;
#[thread_local]
static mut BAR: u32 = 0;
#[allow(named_asm_labels)]
fn main() {
    let foo = unsafe { FOO + BAR };
    println!("Hello, World {}", unsafe { FOO + BAR });

    std::thread::spawn(|| {
        for i in 0..1 {
            println!("hello from thread");
        }
    });
    //panic!("panic test");
    loop {}
}

/*
#[naked]
#[no_mangle]
extern "C" fn _start() -> ! {
    unsafe { asm!("call std_runtime_start", options(noreturn)) }
}
*/
