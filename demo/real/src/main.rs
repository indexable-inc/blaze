use heck::{ToKebabCase, ToSnakeCase, ToUpperCamelCase};

fn main() {
    let v = semver::Version::parse("1.2.3-beta.4+build.5").unwrap();
    let req = semver::VersionReq::parse(">=1.2.0, <2.0.0").unwrap();
    println!("{v} matches {req}: {}", req.matches(&semver::Version::new(1, 4, 0)));
    println!("{} {} {}", "HelloWorldFoo".to_snake_case(), "hello_world".to_upper_camel_case(), "XMLHttpRequest".to_kebab_case());
    println!("{}", itoa::Buffer::new().format(-1234567890123i64));
    println!("{} {:?}", hex::encode(b"blaze!"), hex::decode("7275737464"));
    let mut headers = [httparse::EMPTY_HEADER; 8];
    let mut req = httparse::Request::new(&mut headers);
    let status = req.parse(b"GET /index.html HTTP/1.1\r\nHost: example.com\r\nAccept: */*\r\n\r\n");
    println!("{status:?} {:?} {:?} {}", req.method, req.path, req.headers.len());
}
