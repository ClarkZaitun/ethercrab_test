use proc_macro2::Span;
use std::collections::HashSet;
use syn::{
    punctuated::Punctuated, spanned::Spanned, Expr, ExprArray, ExprLit, Ident, Lit, Meta, Token,
    Type,
};

pub const MY_ATTRIBUTE: &str = "wire";

fn my_attributes(attrs: &[syn::Attribute]) -> impl Iterator<Item = &syn::Attribute> {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident(MY_ATTRIBUTE))
}

pub fn bit_width_attr(attrs: &[syn::Attribute]) -> Result<Option<usize>, syn::Error> {
    let bits = usize_attr(attrs, "bits")?;
    let bytes = usize_attr(attrs, "bytes")?.map(|bytes| bytes * 8);

    if bits.is_some() && bytes.is_some() {
        return Err(syn::Error::new(
            Span::call_site(),
            "'bits' and 'bytes' attribute not allowed at the same time",
        ));
    }

    Ok(bits.or(bytes))
}

pub fn usize_attr(attrs: &[syn::Attribute], search: &str) -> Result<Option<usize>, syn::Error> {
    for attr in my_attributes(attrs) {
        let Ok(nested) = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        else {
            continue;
        };

        for meta in nested {
            match meta {
                Meta::Path(_) | Meta::List(_) => (),
                Meta::NameValue(nv) if nv.path.is_ident(search) => {
                    if let Expr::Lit(ExprLit {
                        lit: Lit::Int(lit), ..
                    }) = &nv.value
                    {
                        return Ok(Some(lit.base10_parse::<usize>()?));
                    }
                }
                Meta::NameValue(_) => (),
            }
        }
    }

    Ok(None)
}

/// Check that all attributes are supported
pub fn all_valid_attrs(attrs: &[syn::Attribute], allowed: &[&str]) -> Result<(), syn::Error> {
    let allowed = allowed
        .iter()
        .map(|s| Ident::new(s, Span::call_site()))
        .collect::<HashSet<_>>();

    let mut idents = HashSet::new();

    for attr in my_attributes(attrs) {
        // Skip other attributes like doc comments etc
        let Ok(nested) = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        else {
            continue;
        };

        for meta in nested {
            let ident = match meta {
                Meta::Path(p) => p.get_ident().cloned().expect("Path identifier required"),
                Meta::List(_) => unreachable!("Unsupported"),
                Meta::NameValue(nv) => nv
                    .path
                    .get_ident()
                    .cloned()
                    .expect("NameValue identifier required"),
            };

            let None = idents.replace(ident.clone()) else {
                panic!("Duplicate attribute found {}", ident);
            };
        }
    }

    let mut bad = idents.difference(&allowed);

    if let Some(first) = bad.next() {
        return Err(syn::Error::new(first.span(), "Invalid attribute"));
    }

    Ok(())
}

pub fn attr_exists(attrs: &[syn::Attribute], search: &str) -> bool {
    for attr in my_attributes(attrs) {
        let Ok(nested) = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        else {
            continue;
        };

        for meta in nested {
            match meta {
                Meta::Path(p) if p.is_ident(search) => return true,
                _ => (),
            }
        }
    }

    false
}

/// 检查结构体或枚举是否具有 `#[repr(packed)]` 属性
///
/// 此函数用于在过程宏展开过程中验证类型是否使用了 `#[repr(packed)]` 内存布局属性。
/// 在处理需要精确内存布局的类型（如网络协议数据结构）时，这是一个重要的检查，
/// 因为 `#[repr(packed)]` 会移除字段间的填充字节，确保类型的内存布局紧凑。
///
/// # 参数
/// * `attrs` - 目标类型的所有属性列表，通常通过 `syn::DeriveInput::attrs` 获取
///
/// # 返回值
/// * `true` - 如果找到 `#[repr(packed)]` 属性
/// * `false` - 如果未找到 `#[repr(packed)]` 属性
///
/// # 实现细节
/// 函数通过以下步骤工作：
/// 1. 遍历类型的所有属性
/// 2. 寻找路径为 "repr" 的属性列表（`#[repr(...)]`）
/// 3. 解析 `repr` 属性的嵌套元数据，检查是否包含 "packed" 标识符
/// 4. 如果找到 "packed"，立即返回 true；否则继续查找或最终返回 false
pub fn has_repr_packed(attrs: &[syn::Attribute]) -> bool {
    // 遍历所有属性，查找 repr 属性
    for attr in attrs {
        match attr.meta.clone() {
            // 匹配形式为 `#[repr(...)]` 的属性列表
            Meta::List(l) if l.path.is_ident("repr") => {
                // 初始化标记变量，用于追踪是否找到 packed 关键字
                let mut has_packed = false;

                // 解析 repr 内部的嵌套元数据
                // 忽略可能的解析错误，继续处理
                let _ = l.parse_nested_meta(|meta| {
                    // 检查元数据是否为 "packed" 标识符
                    if meta.path.is_ident("packed") {
                        has_packed = true;
                    }
                    Ok(())
                });

                // 如果找到 packed 关键字，立即返回 true
                if has_packed {
                    return true;
                }
            }
            // 忽略其他类型的属性
            _ => (),
        }
    }
    // 未找到任何含有 packed 的 repr 属性
    false
}

// pub fn field_is_enum_attr(attrs: &[syn::Attribute]) -> Result<bool, syn::Error> {
//     for attr in my_attributes(attrs) {
//         let Ok(nested) = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
//         else {
//             continue;
//         };

//         for meta in nested {
//             match meta {
//                 Meta::Path(_) => (),
//                 Meta::List(_) => (),
//                 Meta::NameValue(nv) if nv.path.is_ident("ty") => {
//                     if let Expr::Lit(ExprLit {
//                         lit: Lit::Str(s), ..
//                     }) = &nv.value
//                     {
//                         return Ok(s.value() == "enum");
//                     }
//                 }
//                 _ => (),
//             }
//         }
//     }

//     Ok(false)
// }

pub fn enum_repr_ty(attrs: &[syn::Attribute], ident: &Ident) -> Result<Ident, syn::Error> {
    for attr in attrs {
        match attr.meta.clone() {
            Meta::List(l) if l.path.is_ident("repr") => {
                let ty = l.parse_args::<Type>()?;

                if let Type::Path(ty) = ty {
                    return ty
                        .path
                        .get_ident()
                        .cloned()
                        .ok_or_else(|| syn::Error::new(ident.span(), "Repr is not a valid type"));
                }
            }
            _ => (),
        }
    }

    Err(syn::Error::new(
        ident.span(),
        "Enums must have a #[repr()] attribute",
    ))
}

/// Look for `alternatives = [1,2,3]` attribute on enum variant.
pub fn variant_alternatives(attrs: &[syn::Attribute]) -> Result<Vec<i128>, syn::Error> {
    for attr in my_attributes(attrs) {
        let Ok(nested) = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        else {
            continue;
        };

        for meta in nested {
            match meta {
                Meta::Path(_) | Meta::List(_) => (),
                Meta::NameValue(nv) if nv.path.is_ident("alternatives") => {
                    if let Expr::Array(ExprArray { elems, .. }) = &nv.value {
                        return elems
                            .iter()
                            .map(|elem| {
                                let Expr::Lit(ExprLit {
                                    lit: Lit::Int(lit), ..
                                }) = elem.clone()
                                else {
                                    return Err(syn::Error::new(
                                        elem.span(),
                                        "Alternatives must be numbers",
                                    ));
                                };

                                lit.base10_parse::<i128>()
                            })
                            .collect::<Result<Vec<_>, _>>();
                    }
                }
                Meta::NameValue(_) => (),
            }
        }
    }

    Ok(Vec::new())
}

pub fn variant_is_default(attrs: &[syn::Attribute]) -> bool {
    for attr in attrs {
        match attr.meta {
            Meta::Path(ref p) if p.is_ident("default") => return true,
            _ => continue,
        }
    }

    false
}
