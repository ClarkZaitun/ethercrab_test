use crate::parse_struct::StructMeta;
use proc_macro2::{Ident, Span};
use quote::quote;
use std::str::FromStr;
use syn::DeriveInput;

pub fn generate_struct_write(parsed: &StructMeta, input: &DeriveInput) -> proc_macro2::TokenStream {
    let name = input.ident.clone();
    let size_bytes = parsed.width_bits.div_ceil(8);

    let fields_pack = parsed.fields.clone().into_iter().map(|field| {
        let name = field.name;
        let field_ty = field.ty;
        let byte_start = field.bytes.start;
        let bit_start = field.bit_offset;

        if field.skip {
            return quote! {};
        }

        let ty_name = field
            .ty_name
            .unwrap_or_else(|| Ident::new("UnknownTypeStopLookingAtMe", Span::call_site()));

        // Small optimisation
        if ty_name == "u8" || ty_name == "bool" {
            let mask = (2u16.pow(field.bits.len() as u32) - 1) << bit_start;
            let mask = proc_macro2::TokenStream::from_str(&format!("{:#010b}", mask)).unwrap();

            quote! {
                buf[#byte_start] |= ((self.#name as u8) << #bit_start) & #mask;
            }
        }
        // Single byte fields need merging into the other data
        else if field.bytes.len() == 1 {
            let mask = (2u16.pow(field.bits.len() as u32) - 1) << bit_start;
            let mask = proc_macro2::TokenStream::from_str(&format!("{:#010b}", mask)).unwrap();

            quote! {
                let mut field_buf = [0u8; 1];
                let res = <#field_ty as ::ethercrab_wire::EtherCrabWireWrite>::pack_to_slice_unchecked(&self.#name, &mut field_buf)[0];

                buf[#byte_start] |= (res << #bit_start) & #mask;
            }
        }
        // Assumption: multi-byte fields are byte-aligned. This should be validated during parse.
        else {
            let byte_end = field.bytes.end;

            quote! {
                <#field_ty as ::ethercrab_wire::EtherCrabWireWrite>::pack_to_slice_unchecked(&self.#name, &mut buf[#byte_start..#byte_end]);
            }
        }
    });

    quote! {
        impl ::ethercrab_wire::EtherCrabWireWrite for #name {
            fn pack_to_slice_unchecked<'buf>(&self, buf: &'buf mut [u8]) -> &'buf [u8] {
                let buf = match buf.get_mut(0..#size_bytes) {
                    Some(buf) => buf,
                    None => unreachable!()
                };

                unsafe {
                    buf.as_mut_ptr().write_bytes(0u8, buf.len());
                }

                #(#fields_pack)*

                buf
            }

            fn packed_len(&self) -> usize {
                #size_bytes
            }
        }

        impl ::ethercrab_wire::EtherCrabWireWriteSized for #name {
            fn pack(&self) -> Self::Buffer {
                let mut buf = [0u8; #size_bytes];

                <Self as ::ethercrab_wire::EtherCrabWireWrite>::pack_to_slice_unchecked(self, &mut buf);

                buf
            }
        }
    }
}

pub fn generate_struct_read(parsed: &StructMeta, input: &DeriveInput) -> proc_macro2::TokenStream {
    let name = input.ident.clone();
    let size_bytes = parsed.width_bits.div_ceil(8);

    let fields_unpack = parsed.fields.clone().into_iter().map(|field| {
        let ty = field.ty;
        let name = field.name;
        let byte_start = field.bytes.start;
        let bit_start = field.bit_offset;
        let ty_name = field
            .ty_name
            .unwrap_or_else(|| Ident::new("UnknownTypeStopLookingAtMe", Span::call_site()));

        if field.skip {
            return quote! {
                #name: Default::default()
            }
        }

        if field.bits.len() <= 8 {
            let mask = (2u16.pow(field.bits.len() as u32) - 1) << bit_start;
            let mask =
                proc_macro2::TokenStream::from_str(&format!("{:#010b}", mask)).unwrap();

            if ty_name == "bool" {
                quote! {
                    #name: ((buf.get(#byte_start).ok_or(::ethercrab_wire::WireError::ReadBufferTooShort)? & #mask) >> #bit_start) > 0
                }
            }
            // Small optimisation
            else if ty_name == "u8" {
                quote! {
                    #name: (buf.get(#byte_start).ok_or(::ethercrab_wire::WireError::ReadBufferTooShort)? & #mask) >> #bit_start
                }
            }
            // Anything else will be a struct or an enum
            else {
                quote! {
                    #name: {
                        let masked = (buf.get(#byte_start).ok_or(::ethercrab_wire::WireError::ReadBufferTooShort)? & #mask) >> #bit_start;

                        <#ty as ::ethercrab_wire::EtherCrabWireRead>::unpack_from_slice(&[masked])?
                    }
                }
            }
        }
        // Assumption: multi-byte fields are byte-aligned. This must be validated during parse.
        else {
            let start_byte = field.bytes.start;
            let end_byte = field.bytes.end;

            quote! {
                #name: <#ty as ::ethercrab_wire::EtherCrabWireRead>::unpack_from_slice(buf.get(#start_byte..#end_byte).ok_or(::ethercrab_wire::WireError::ReadBufferTooShort)?)?
            }
        }
    });

    quote! {
        impl ::ethercrab_wire::EtherCrabWireRead for #name {
            // 接收一个字节切片 buf 作为参数，尝试把切片里的数据反序列化为 Self 类型（即结构体实例），返回 Result 类型，成功时返回结构体实例，失败时返回 ::ethercrab_wire::WireError 类型的错误
            // 实现类似C的字节流强制转换为结构体
            fn unpack_from_slice(buf: &[u8]) -> Result<Self, ::ethercrab_wire::WireError> {
                let buf = buf.get(0..#size_bytes).ok_or(::ethercrab_wire::WireError::ReadBufferTooShort)?;

                Ok(Self {
                    #(#fields_unpack),*
                })
            }
        }
    }
}

// 生成实现 ::ethercrab_wire::EtherCrabWireSized trait 的代码
// parsed: &StructMeta：StructMeta 类型的引用，包含结构体的解析元数据。
// input: &DeriveInput：DeriveInput 类型的引用，代表 #[derive] 属性的输入信息。
// 返回值类型为 proc_macro2::TokenStream，即生成的代码片段的标记流。
pub fn generate_sized_impl(parsed: &StructMeta, input: &DeriveInput) -> proc_macro2::TokenStream {
    let name = input.ident.clone();
    let size_bytes = parsed.width_bits.div_ceil(8);

    // 生成代码
    quote! { // quote 库提供的宏，用于生成 Rust 代码
        impl ::ethercrab_wire::EtherCrabWireSized for #name {//# 是 quote 宏特有的语法，用于把 Rust 变量嵌入到生成的代码片段中
            const PACKED_LEN: usize = #size_bytes; // 给实现 EtherCrabWireSized trait 的类型设置PACKED_LEN

            type Buffer = [u8; #size_bytes]; //定义关联类型 Buffer，为长度为 size_bytes 的 u8 数组

            // 返回一个全为 0 的 Buffer 类型数组，长度已经确定
            fn buffer() -> Self::Buffer {
                [0u8; #size_bytes]
            }
        }
    }
}
