use crate::help::{all_valid_attrs, attr_exists, bit_width_attr, usize_attr};
use std::ops::Range;
use syn::{DataStruct, DeriveInput, Fields, FieldsNamed, Ident, Type, Visibility};

// 元数据结构，用于描述需要被序列化/反序列化的 Rust 结构体的布局信息
#[derive(Clone)]
pub struct StructMeta {
    /// Width in bits on the wire.
    pub width_bits: usize, // 整个结构体在二进制数据流中占用的总比特数

    pub fields: Vec<FieldMeta>, // 结构体中每个字段的元数据信息
}

#[derive(Clone)]
pub struct FieldMeta {
    #[allow(unused)]
    pub vis: Visibility, // 字段的可见性(public/private等)
    pub name: Ident, // 字段名称标识符
    pub ty: Type,    //字段类型信息
    // Will be None for arrays
    pub ty_name: Option<Ident>, // 字段类型名称(仅适用于简单类型路径)
    #[allow(unused)]
    pub bit_start: usize, // 字段在比特流中的起始/结束位置
    #[allow(unused)]
    pub bit_end: usize,
    #[allow(unused)]
    pub byte_start: usize, // 字段在字节流中的起始/结束位置
    #[allow(unused)]
    pub byte_end: usize,
    /// Offset of the starting bit in the starting byte.
    pub bit_offset: usize, // 字段在起始字节中的比特偏移量

    pub bits: Range<usize>,  // 字段占用的比特范围
    pub bytes: Range<usize>, // 字段占用的字节范围

    #[allow(unused)]
    pub pre_skip: Option<usize>, // 字段前后的填充比特数(可选)
    #[allow(unused)]
    pub post_skip: Option<usize>,

    pub skip: bool, //  是否跳过该字段(不进行序列化/反序列化)
}

pub fn parse_struct(
    s: DataStruct,
    DeriveInput { attrs, ident, .. }: DeriveInput,
) -> syn::Result<StructMeta> {
    // --- Struct attributes

    all_valid_attrs(&attrs, &["bits", "bytes"])?;

    let width = bit_width_attr(&attrs)?;

    let Some(width) = width else {
        return Err(syn::Error::new(
            ident.span(),
            "Struct total bit width is required, e.g. #[wire(bits = 32)]",
        ));
    };

    // --- Fields

    let Fields::Named(FieldsNamed { named: fields, .. }) = s.fields else {
        return Err(syn::Error::new(
            ident.span(),
            "Only structs with named fields can be derived.",
        ));
    };

    let mut total_field_width = 0;

    let mut field_meta = Vec::new();

    for field in fields {
        all_valid_attrs(
            &field.attrs,
            &[
                "bits",
                "bytes",
                "skip",
                "pre_skip",
                "pre_skip_bytes",
                "post_skip",
                "post_skip_bytes",
            ],
        )?;

        // Unwrap: this is a named-field struct so the field will always have a name.
        let field_name = field.ident.unwrap();
        let field_width = bit_width_attr(&field.attrs)?;

        // Whether to ignore this field when sending AND receiving
        let skip = attr_exists(&field.attrs, "skip");

        let pre_skip = usize_attr(&field.attrs, "pre_skip")?
            .or(usize_attr(&field.attrs, "pre_skip_bytes")?.map(|bytes| bytes * 8))
            .filter(|_| !skip);

        let post_skip = usize_attr(&field.attrs, "post_skip")?
            .or(usize_attr(&field.attrs, "post_skip_bytes")?.map(|bytes| bytes * 8))
            .filter(|_| !skip);

        if let Some(skip) = pre_skip {
            total_field_width += skip;
        }

        let bit_start = total_field_width;
        let bit_end = field_width.map_or(total_field_width, |w| total_field_width + w);
        let byte_start = bit_start / 8;
        let byte_end = bit_end.div_ceil(8);
        let bytes = byte_start..byte_end;
        let bit_offset = bit_start % 8;
        let bits = bit_start..bit_end;

        let ty_name = match field.ty.clone() {
            Type::Path(path) => path.path.get_ident().cloned(),
            _ => None,
        };

        let meta = FieldMeta {
            name: field_name,
            vis: field.vis,
            ty: field.ty,
            ty_name,

            bits,
            bytes,

            bit_start,
            bit_end,
            byte_start,
            byte_end,

            bit_offset,

            pre_skip,
            post_skip,

            skip,
        };

        // Validation if we're not skipping this field
        if !skip {
            let Some(field_width) = field_width else {
                return Err(syn::Error::new(
                    meta.name.span(),
                    "Field must have a width attribute, e.g. #[wire(bits = 4)]",
                ));
            };

            if meta.bytes.len() > 1 && (bit_offset > 0 || field_width % 8 > 0) {
                return Err(syn::Error::new(
                    meta.name.span(),
                    format!("Multibyte fields must be byte-aligned at start and end. Current bit position {}", total_field_width),
                ));
            }

            if meta.bits.len() < 8 && meta.bytes.len() > 1 {
                return Err(syn::Error::new(
                    meta.name.span(),
                    "Fields smaller than 8 bits may not cross byte boundaries",
                ));
            }

            total_field_width += field_width;
        }

        if let Some(skip) = post_skip {
            total_field_width += skip;
        }

        field_meta.push(meta);
    }

    if total_field_width != width {
        return Err(syn::Error::new(
            ident.span(),
            format!(
                "Total field width is {}, expected {} from struct definition",
                total_field_width, width
            ),
        ));
    }

    Ok(StructMeta {
        width_bits: width,
        fields: field_meta,
    })
}
