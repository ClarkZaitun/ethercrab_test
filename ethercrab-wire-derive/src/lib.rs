//! Derive attributes for [`ethercrab-wire`].
//!
//! # Experimental
//!
//! This crate is in its early stages and may contain bugs or publish breaking changes at any time.
//! It is in use by [`ethercrab`] and is well exercised there, but please use with caution in your
//! own code.
//!
//! These derives support both structs with bit- and (multi)byte-sized fields for structs, as well
//! as enums with optional catch-all variant.
//!
//! # Supported attributes
//!
//! ## Structs
//!
//! - `#[wire(bits = N)]` OR `#[wire(bytes = N)]`
//!
//!   The size of this struct when packed on the wire. These attributes may not be present at the
//!   same time.
//!
//! ## Struct fields
//!
//! - `#[wire(bits = N)]` OR `#[wire(bytes = N)]`
//!
//!   How many bytes this field consumes on the wire. These attributes may not be present at the
//!   same time.
//!
//! - `#[wire(pre_skip = N)]` OR `#[wire(pre_skip_bytes = N)]`
//!
//!   Skip one or more whole bytes before or after this field in the packed representation.
//!
//! - `#[wire(post_skip = N)]` OR `#[wire(post_skip_bytes = N)]`
//!
//!   How many bits or bytes to skip in the raw data **after** this field.
//!
//!   These attributes are only applicable to fields that are less than 8 bits wide.
//!
//! ## Enums
//!
//! Enums must have a `#[repr()]` attribute, as well as implement the `Copy` trait.
//!
//! ## Enum discriminants
//!
//! Enum discriminants may not contain fields.
//!
//! - `#[wire(alternatives = [])]`
//!
//!   A discriminant with this attribute will be parsed successfully if either its direct value or
//!   any of the listed alternatives are found in the input data.
//!
//!   The discriminant value is used when packing _to_ the wire.
//!
//! - `#[wire(catch_all)]`
//!
//!   Apply this once to a discriminant with a single unnamed field the same type as the enum's
//!   `#[repr()]` to catch any unrecognised values.
//!
//! # Examples
//!
//! ## A struct with both bit fields and multi-byte fields.
//!
//! ```rust
//! #[derive(ethercrab_wire::EtherCrabWireReadWrite)]
//! #[wire(bytes = 4)]
//! struct Mixed {
//!     #[wire(bits = 1)]
//!     one_bit: u8,
//!     #[wire(bits = 2)]
//!     two_bits: u8,
//!
//!     // Fields that are 8 bits or larger must be byte aligned, so we skip the two remaining bits
//!     // of the previous byte with `post_skip`.
//!     #[wire(bits = 3, post_skip = 2)]
//!     three_bits: u8,
//!
//!     /// Whole `u8`
//!     #[wire(bytes = 1)]
//!     one_byte: u8,
//!
//!     /// Whole `u16`
//!     #[wire(bytes = 2)]
//!     one_word: u16,
//! }
//! ```
//!
//! ## Enum with catch all discriminant and alternatives
//!
//! ```rust
//! # use ethercrab_wire::EtherCrabWireRead;
//! # #[derive(PartialEq, Debug)]
//! #[derive(Copy, Clone, ethercrab_wire::EtherCrabWireReadWrite)]
//! #[repr(u8)]
//! enum OneByte {
//!     Foo = 0x01,
//!     #[wire(alternatives = [ 3, 4, 5, 6 ])]
//!     Bar = 0x02,
//!     Baz = 0x07,
//!     Quux = 0xab,
//!     #[wire(catch_all)]
//!     Unknown(u8),
//! }
//!
//! // Normal discriminant
//! assert_eq!(OneByte::unpack_from_slice(&[0x07]), Ok(OneByte::Baz));
//!
//! // Alternative value for `Bar`
//! assert_eq!(OneByte::unpack_from_slice(&[0x05]), Ok(OneByte::Bar));
//!
//! // Catch all
//! assert_eq!(OneByte::unpack_from_slice(&[0xaa]), Ok(OneByte::Unknown(0xaa)));
//! ```
//!
//! # Struct field alignment
//!
//! Struct fields of 1 byte or more MUST be byte-aligned. For example, the following struct will be
//! rejected due to `bar` being 5 bits "early":
//!
//! ```rust,compile_fail
//! #[derive(ethercrab_wire::EtherCrabWireReadWrite)]
//! #[wire(bytes = 2)]
//! struct Broken {
//!     #[wire(bits = 3)]
//!     foo: u8,
//!
//!     // There are 5 bits here unaccounted for
//!
//!     #[wire(bytes = 1)]
//!     bar: u8,
//! }
//! ```
//!
//! This can easily be fixed by using the `pre_skip` or `post_skip` attributes to realign the next
//! field to 8 bits (or skip whole bytes of the input data):
//!
//! ```rust
//! #[derive(ethercrab_wire::EtherCrabWireReadWrite)]
//! #[wire(bytes = 2)]
//! struct Fixed {
//!     #[wire(bits = 3, post_skip = 5)]
//!     foo: u8,
//!     #[wire(bytes = 1)]
//!     bar: u8,
//! }
//! ```
//!
//! A field in the middle of a byte can be written as such, maintaining 8 bit alignment:
//!
//! ```rust
//! #[derive(ethercrab_wire::EtherCrabWireReadWrite)]
//! #[wire(bytes = 1)]
//! struct Middle {
//!     #[wire(pre_skip = 2, bits = 3, post_skip = 3)]
//!     foo: u8,
//! }
//! ```
//!
//! [`ethercrab`]: https://docs.rs/ethercrab
//! [`ethercrab-wire`]: https://docs.rs/ethercrab-wire

#![deny(missing_docs)]
#![deny(missing_copy_implementations)]
#![deny(trivial_casts)]
#![deny(trivial_numeric_casts)]
#![deny(unused_import_braces)]
#![deny(unused_qualifications)]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::private_intra_doc_links)]

mod generate_enum;
mod generate_struct;
mod help;
mod parse_enum;
mod parse_struct;

use generate_enum::{generate_enum_read, generate_enum_write};
use generate_struct::{generate_sized_impl, generate_struct_read, generate_struct_write};
use parse_enum::parse_enum;
use parse_struct::parse_struct;
use proc_macro::TokenStream;
use syn::{parse_macro_input, Data, DeriveInput};

/// Items that can be written to and read from the wire.
///
/// Please see the [crate documentation](index.html) for examples and supported
/// attributes.
///
/// For write-only items, see [`EtherCrabWireWrite`]. For read-only items, see
/// [`EtherCrabWireRead`].
#[proc_macro_derive(EtherCrabWireReadWrite, attributes(wire))]
// proc_macro_derive定义派生宏EtherCrabWireReadWrite
// attributes(wire)参数指定了宏在处理过程中可以识别的自定义属性。wire 是自定义属性名，用户能在结构体、枚举、字段或枚举变体上使用 #[wire(...)] 形式的属性，为宏提供额外信息，指导代码生成
pub fn ether_crab_wire(input: TokenStream) -> TokenStream {
    // input: TokenStream 输入是编译器传递的待处理代码标记流
    // 将输入的 TokenStream 解析为 DeriveInput 结构体，该结构体包含了 #[derive] 属性所修饰的类型定义信息，如类型名、数据类型（枚举、结构体等）
    let input = parse_macro_input!(input as DeriveInput);

    // 根据数据类型处理
    let res = match input.clone().data {
        // 解析枚举定义，返回解析结果
        Data::Enum(e) => parse_enum(e, input.clone()).map(|parsed| {
            let mut tokens = generate_enum_write(parsed.clone(), &input, false); // 生成枚举类型的写入代码

            tokens.extend(generate_enum_read(parsed, &input)); // 生成枚举类型的读取代码

            tokens
        }),
        // 解析结构体定义，返回解析结果
        Data::Struct(s) => parse_struct(s, input.clone()).map(|parsed| {
            let mut tokens = generate_struct_write(&parsed, &input); // 生成结构体类型的写入代码

            tokens.extend(generate_struct_read(&parsed, &input)); // 生成结构体类型的读取代码

            tokens.extend(generate_sized_impl(&parsed, &input)); // 生成 EtherCrabWireSized trait 的实现代码
                                                                 //将生成的写入、读取和 EtherCrabWireSized 实现代码合并到 tokens 中

            tokens
        }),
        // 返回一个编译错误，提示联合体不被支持
        Data::Union(_) => Err(syn::Error::new(
            input.ident.span(),
            "Unions are not supported", //如果生成代码过程中出现错误（Err 分支），将错误信息转换为编译错误的 TokenStream 并返回，使编译器在编译时输出错误信息
        )),
    };

    // 错误处理
    let res = match res {
        Ok(res) => res,
        Err(e) => return e.to_compile_error().into(),
    };

    TokenStream::from(res)
}

/// Items that can only be read from the wire.
///
/// Please see the [crate documentation](index.html) for examples and supported attributes.
///
/// For read/write items, see [`EtherCrabWireReadWrite`]. For write-only items, see
/// [`EtherCrabWireWrite`].
#[proc_macro_derive(EtherCrabWireRead, attributes(wire))]
pub fn ether_crab_wire_read(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    let res = match input.clone().data {
        Data::Enum(e) => {
            parse_enum(e, input.clone()).map(|parsed| generate_enum_read(parsed, &input))
        }
        Data::Struct(s) => parse_struct(s, input.clone()).map(|parsed| {
            let mut tokens = generate_struct_read(&parsed, &input);

            tokens.extend(generate_sized_impl(&parsed, &input));

            tokens
        }),
        Data::Union(_) => Err(syn::Error::new(
            input.ident.span(),
            "Unions are not supported",
        )),
    };

    let res = match res {
        Ok(res) => res,
        Err(e) => return e.to_compile_error().into(),
    };

    TokenStream::from(res)
}

/// Items that can only be written to the wire.
///
/// Please see the [crate documentation](index.html) for examples and supported attributes.
///
/// For read/write items, see [`EtherCrabWireReadWrite`]. For read-only items, see
/// [`EtherCrabWireRead`].
#[proc_macro_derive(EtherCrabWireWrite, attributes(wire))]
pub fn ether_crab_wire_write(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    let res = match input.clone().data {
        Data::Enum(e) => {
            parse_enum(e, input.clone()).map(|parsed| generate_enum_write(parsed, &input, true))
        }
        Data::Struct(s) => parse_struct(s, input.clone()).map(|parsed| {
            let mut tokens = generate_struct_write(&parsed, &input);

            tokens.extend(generate_sized_impl(&parsed, &input));

            tokens
        }),
        Data::Union(_) => Err(syn::Error::new(
            input.ident.span(),
            "Unions are not supported",
        )),
    };

    let res = match res {
        Ok(res) => res,
        Err(e) => return e.to_compile_error().into(),
    };

    TokenStream::from(res)
}

#[cfg(test)]
mod tests {
    #[test]
    fn trybuild_cases() {
        let t = trybuild::TestCases::new();

        t.compile_fail("ui/*.rs");
    }
}
