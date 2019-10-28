#[derive(Clone, Debug)]
struct Foo {
    pub x: i32,
    pub y: i32,
}

fn foo<T>() -> T {
    unimplemented!();
    foo::<i32>();
}

// comment
fn main() {
    println!("Hello, {}!", 92);

    let mut vec = Vec::new();
    if true {
        vec.push(Foo { x: 0, y: 1 });
    }
    unsafe { vec.set_len(0); }

    let mut x = 42;
    let y = &mut x;
    let z = &y;

    y;
}