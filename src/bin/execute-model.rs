use std::io;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.as_slice() == ["--version"] {
        println!("execute-model {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.as_slice() == ["--help"] {
        println!("{}", flow::models::help());
        return;
    }

    if let Err(error) = flow::models::run(&args, &mut io::stdin().lock(), &mut io::stdout().lock())
    {
        // Emit a static failure class and, only for HTTP status, its numeric
        // code. Never print the error chain, response body, or credential.
        eprintln!("execute-model: {}", flow::models::diagnostic(&error));
        std::process::exit(1);
    }
}
