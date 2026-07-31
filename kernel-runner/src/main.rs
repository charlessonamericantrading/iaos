use bootloader::DiskImageBuilder;
use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let (Some(kernel_path), Some(out_dir)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: kernel-runner <path-to-kernel-elf> <output-dir>");
        return ExitCode::FAILURE;
    };

    let builder = DiskImageBuilder::new(PathBuf::from(kernel_path));
    let out_dir = PathBuf::from(out_dir);

    let bios_path = out_dir.join("agentos-bios.img");
    if let Err(e) = builder.create_bios_image(&bios_path) {
        eprintln!("[kernel-runner] failed to create BIOS image: {e:?}");
        return ExitCode::FAILURE;
    }
    println!("[kernel-runner] BIOS image:  {}", bios_path.display());

    let uefi_path = out_dir.join("agentos-uefi.img");
    match builder.create_uefi_image(&uefi_path) {
        Ok(()) => println!("[kernel-runner] UEFI image:  {}", uefi_path.display()),
        Err(e) => println!("[kernel-runner] UEFI image skipped: {e:?}"),
    }

    ExitCode::SUCCESS
}
