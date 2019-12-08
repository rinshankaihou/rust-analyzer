use std::string::String;
pub struct MilkTea<'a> {
    pub tea 'a : &str,
    pub sugar: u32,
    pub ice: u32,
}

trait Foo {
    fn foo() -> i32;
}

struct Bar;

impl Bar {
    fn foo() -> i32 {
        20
    }
}

impl Foo for Bar {
    fn foo() -> i32 {
        10
    }
}

fn eg1() -> String {
    let a = Bar::foo();
    let b =  <Bar as Foo>::foo();
    "hohoho".to_string()
}

fn main() {
    eg1();
    let balance = -37;
    let sugar : u32 = 30;
    let ice : u32 = 0;
    let price : u64 = 10000000;
    let str1 : String = "Oolong milktea".to_string();
    let str2 : String = "30% sugar no ice".to_string();
    let thing = sugar + ice;
    
    let tea = "tea";
    let tea1= "tea1";
    let tea2= "shadow tea2";
    // let tea2= str1 + str2;
    // let tea_alter = str1 + str2;
    // let tea_alter = str1 + str2 + str2;
    let tea_alter = "hahahaha";
    let mt = MilkTea{tea, sugar, ice,};
    let mt2 = MilkTea{tea1, sugar, ice};
    let mt3 = MilkTea{tea2, sugar, ice};
    let balance = balance - price;
    format!("{a} AHHHHHHH!", a = tea);
}