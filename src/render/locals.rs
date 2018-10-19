use crate::rustc::mir::{self, interpret::EvalErrorKind};
use crate::rustc::ty::{
    layout::{Abi, Size},
    ParamEnv, TyKind, TyS, TypeAndMut,
};

use miri::{
    Allocation, EvalResult, Frame, LocalValue, OpTy, Operand, Place, Pointer, PointerArithmetic,
    Scalar, ScalarMaybeUndef, Value,
};

use horrorshow::prelude::*;
use horrorshow::Template;

use crate::EvalContext;

pub fn render_locals<'a, 'tcx: 'a>(
    ecx: &EvalContext<'a, 'tcx>,
    frame: &Frame<'tcx, 'tcx>,
) -> String {
    let &Frame {
        instance,
        ref locals,
        ref mir,
        ref return_place,
        ..
    } = frame;

    //               name    ty      alloc        val     style
    let locals: Vec<(String, String, Option<u64>, String, &str)> = mir
        .local_decls
        .iter_enumerated()
        .map(|(id, local_decl)| {
            let name = local_decl
                .name
                .map(|n| n.as_str().to_string())
                .unwrap_or_else(String::new);
            let ty = ecx.monomorphize(local_decl.ty, instance.substs);

            let op_ty = if id == mir::RETURN_PLACE {
                return_place.map(|p| {
                    ecx.place_to_op(p).unwrap()
                })
            } else {
                match *locals.get(id).unwrap() /* never None, because locals has a entry for every defined local */ {
                    LocalValue::Dead => None,
                    LocalValue::Live(op) => {
                        if ecx.frame() as *const _ == frame as *const _ {
                            Some(ecx.eval_operand(&mir::Operand::Move(mir::Place::Local(id)), None).unwrap())

                        } else {
                            None // TODO Above doesn't work for non top frames
                        }
                        //Some(OpTy { op, layout: ecx.tcx.layout_of(ParamEnv::reveal_all().and(ty)).unwrap() })
                    }
                }
            };

            let (alloc, val, style) = match op_ty {
                None => (None, "&lt;uninit&gt;".to_owned(), "font-size: 0;"),
                Some(op_ty) => {
                    match print_operand(ecx, op_ty) {
                        Ok((alloc, text)) => (alloc, text, ""),
                        Err(()) => (None, "&lt;error&gt;".to_owned(), "color: red;"),
                    }
                }
            };
            (name, ty.to_string(), alloc, val, style)
        })
        .collect();

    let (arg_count, var_count, tmp_count) = (
        mir.args_iter().count(),
        mir.vars_iter().count(),
        mir.temps_iter().count(),
    );

    (html! {
        table(border="1") {
            tr {
                td(width="20px");
                th { : "id" }
                th { : "name" }
                th { : "alloc" }
                th { : "memory" }
                th { : "type" }
            }
            @ for (i, &(ref name, ref ty, alloc, ref text, ref style)) in locals.iter().enumerate() {
                tr(style=style) {
                    @if i == 0 {
                        th(rowspan=1) { span(class="vertical") { : "Return" } }
                    } else if i == 1 && arg_count != 0 {
                        th(rowspan=arg_count) { span(class="vertical") { : "Arguments" } }
                    } else if i == arg_count + 1 {
                        th(rowspan=var_count) { span(class="vertical") { : "Variables" } }
                    } else if i == var_count + arg_count + 1 {
                        th(rowspan=tmp_count) { span(class="vertical") { : "Temporaries" } }
                    }
                    td { : format!("_{}", i) }
                    td { : name }
                    @if let Some(alloc) = alloc {
                        td { : alloc.to_string() }
                    } else {
                        td;
                    }
                    td { : Raw(text) }
                    td { : ty }
                }
            }
        }
    }).into_string()
        .unwrap()
}

fn print_scalar_maybe_undef(val: ScalarMaybeUndef) -> String {
    match val {
        ScalarMaybeUndef::Undef => "&lt;undef &gt;".to_string(),
        ScalarMaybeUndef::Scalar(val) => print_scalar(val),
    }
}

fn print_scalar(val: Scalar) -> String {
    match val {
        Scalar::Ptr(ptr) => format!(
            "<a href=\"/ptr/{alloc}/{offset}\">Pointer({alloc})[{offset}]</a>",
            alloc = ptr.alloc_id.0,
            offset = ptr.offset.bytes()
        ),
        Scalar::Bits { bits, size } => {
            if size == 0 {
                "&lt;zst&gt;".to_string()
            } else {
                format!("0x{:0width$X}", bits, width = (size as usize) / 8)
            }
        }
    }
}

fn pp_operand<'a, 'tcx: 'a>(
    ecx: &EvalContext<'a, 'tcx>,
    op_ty: OpTy<'tcx>,
) -> EvalResult<'tcx, String> {
    match op_ty.layout.ty.sty {
        TyKind::RawPtr(TypeAndMut {
            ty: &TyS {
                sty: TyKind::Str, ..
            },
            ..
        })
        | TyKind::Ref(
            _,
            &TyS {
                sty: TyKind::Str, ..
            },
            _,
        ) => {
            if let Operand::Immediate(val) = *op_ty {
                if let Value::ScalarPair(
                    ScalarMaybeUndef::Scalar(Scalar::Ptr(ptr)),
                    ScalarMaybeUndef::Scalar(Scalar::Bits { bits: len, .. }),
                ) = val
                {
                    if let Ok(allocation) = ecx.memory.get(ptr.alloc_id) {
                        let offset = ptr.offset.bytes();
                        if (offset as u128) < allocation.bytes.len() as u128 {
                            let alloc_bytes = &allocation.bytes[offset as usize
                                ..(offset as usize)
                                    .checked_add(len as usize)
                                    .ok_or(EvalErrorKind::AssumptionNotHeld)?];
                            let s = String::from_utf8_lossy(alloc_bytes);
                            return Ok(format!("\"{}\"", s));
                        }
                    }
                }
            }
        }
        TyKind::Adt(adt_def, _substs) => {
            if let Operand::Immediate(Value::Scalar(ScalarMaybeUndef::Undef)) = *op_ty {
                Err(EvalErrorKind::AssumptionNotHeld)?;
            }

            let variant = if adt_def.variants.len() == 1 {
                0
            } else {
                ecx.read_discriminant(op_ty)?.1
            };

            let adt_fields = &adt_def.variants[variant].fields;

            let should_collapse = adt_fields.len() > 1;

            //println!("{:?} {:?} {:?}", val, ty, adt_def.variants);
            let mut pretty = ecx
                .tcx
                .absolute_item_path_str(adt_def.did)
                .replace("<", "&lt;")
                .replace(">", "&gt;")
                .to_string();

            if adt_def.is_enum() {
                pretty.push_str("::");
                pretty.push_str(&*adt_def.variants[variant].name.as_str());
            }
            pretty.push_str(" { ");

            if should_collapse {
                pretty.push_str("<details>");
            }

            for (i, adt_field) in adt_fields.iter().enumerate() {
                let field_pretty: EvalResult<String> = try {
                    let field_op_ty = ecx.operand_field(op_ty, i as u64)?;
                    pp_operand(ecx, field_op_ty)?
                };

                pretty.push_str(&format!(
                    "{}: {}, ",
                    adt_field.ident.as_str(),
                    match field_pretty {
                        Ok(field_pretty) => field_pretty,
                        Err(_err) => "<span style='color: red;'>&lt;err&gt;</span>".to_string(),
                    }
                ));
                if should_collapse {
                    pretty.push_str("<br>");
                }
            }

            if should_collapse {
                pretty.push_str("</details>");
            }

            pretty.push_str("}");
            println!("pretty adt: {}", pretty);
            return Ok(pretty);
        }
        _ => {}
    }

    if op_ty.layout.size.bytes() == 0 {
        Err(EvalErrorKind::AssumptionNotHeld)?;
    }
    if let Abi::Scalar(_) = op_ty.layout.abi {
    } else {
        Err(EvalErrorKind::AssumptionNotHeld)?;
    }
    let scalar = ecx.read_scalar(op_ty)?;
    if let ScalarMaybeUndef::Scalar(Scalar::Ptr(_)) = &scalar {
        return Ok(print_scalar_maybe_undef(scalar)); // If the value is a ptr, print it
    }
    let bits = scalar.to_bits(op_ty.layout.size)?;
    match op_ty.layout.ty.sty {
        TyKind::Bool => {
            if bits == 0 {
                Ok("false".to_string())
            } else if bits == 1 {
                Ok("true".to_string())
            } else {
                Err(EvalErrorKind::AssumptionNotHeld.into())
            }
        }
        TyKind::Char if bits < ::std::char::MAX as u128 => {
            let chr = ::std::char::from_u32(bits as u32).unwrap();
            if chr.is_ascii() {
                Ok(format!("'{}'", chr))
            } else {
                Err(EvalErrorKind::AssumptionNotHeld.into())
            }
        }
        TyKind::Uint(_) => Ok(format!("{0}", bits)),
        TyKind::Int(_) => Ok(format!(
            "{0}",
            ::miri::sign_extend(bits, op_ty.layout.size) as i128
        )),
        TyKind::Float(float_ty) => {
            use crate::syntax::ast::FloatTy::*;
            match float_ty {
                F32 if bits < ::std::u32::MAX as u128 => {
                    Ok(format!("{}", <f32>::from_bits(bits as u32)))
                }
                F64 if bits < ::std::u64::MAX as u128 => {
                    Ok(format!("{}", <f64>::from_bits(bits as u64)))
                }
                _ => Err(EvalErrorKind::AssumptionNotHeld.into()),
            }
        }
        _ => Err(EvalErrorKind::AssumptionNotHeld.into()),
    }
}

pub fn print_operand<'a, 'tcx: 'a>(
    ecx: &EvalContext<'a, 'tcx>,
    op_ty: OpTy<'tcx>,
) -> Result<(Option<u64>, String), ()> {
    let pretty = pp_operand(ecx, op_ty);

    let (alloc, txt) = match *op_ty {
        Operand::Indirect(place) => {
            let size: u64 = op_ty.layout.size.bytes();
            if place.meta.is_none() {
                let ptr = place.to_scalar_ptr_align().0;
                let (alloc, txt, _len) = print_ptr(ecx, ptr, Some(size))?;
                (alloc, txt)
            } else {
                (None, format!("{:?}", place)) // FIXME better printing for unsized locals
            }
        }
        Operand::Immediate(Value::Scalar(scalar)) => (None, print_scalar_maybe_undef(scalar)),
        Operand::Immediate(Value::ScalarPair(val, extra)) => (
            None,
            format!(
                "{}, {}",
                print_scalar_maybe_undef(val),
                print_scalar_maybe_undef(extra)
            ),
        ),
    };
    let txt = if let Ok(pretty) = pretty {
        format!("{} ({})", pretty, txt)
    } else {
        txt
    };
    Ok((alloc, txt))
}

pub fn print_ptr(
    ecx: &EvalContext,
    ptr: Scalar,
    size: Option<u64>,
) -> Result<(Option<u64>, String, u64), ()> {
    let ptr = ptr.to_ptr().map_err(|_| ())?;
    match (ecx.memory().get(ptr.alloc_id), ecx.memory().get_fn(ptr)) {
        (Ok(alloc), Err(_)) => {
            let s = print_alloc(ecx.memory().pointer_size().bytes(), ptr, alloc, size);
            Ok((Some(ptr.alloc_id.0), s, alloc.bytes.len() as u64))
        }
        (Err(_), Ok(_)) => {
            // FIXME: print function name
            Ok((None, "function pointer".to_string(), 16))
        }
        (Err(_), Err(_)) => Err(()),
        (Ok(_), Ok(_)) => unreachable!(),
    }
}

pub fn print_alloc(ptr_size: u64, ptr: Pointer, alloc: &Allocation, size: Option<u64>) -> String {
    use std::fmt::Write;
    let end = size
        .map(|s| s + ptr.offset.bytes())
        .unwrap_or(alloc.bytes.len() as u64);
    let mut s = String::new();
    let mut i = ptr.offset.bytes();
    while i < end {
        if let Some((_tag, reloc)) = alloc.relocations.get(&Size::from_bytes(i)) {
            i += ptr_size;
            write!(&mut s,
                "<a style=\"text-decoration: none\" href=\"/ptr/{alloc}/{offset}\">┠{nil:─<wdt$}┨</a>",
                alloc = reloc.0,
                offset = ptr.offset.bytes(),
                nil = "",
                wdt = (ptr_size * 2 - 2) as usize,
            ).unwrap();
        } else {
            if alloc
                .undef_mask
                .is_range_defined(Size::from_bytes(i), Size::from_bytes(i + 1))
                .is_ok()
            {
                write!(&mut s, "{:02x}", alloc.bytes[i as usize] as usize).unwrap();
            } else {
                let ub_chars = [
                    '∅', '∆', '∇', '∓', '∞', '⊙', '⊠', '⊘', '⊗', '⊛', '⊝',
                    '⊡', '⊠',
                ];
                let c1 = (ptr.alloc_id.0 * 769 + i as u64 * 5689) as usize % ub_chars.len();
                let c2 = (ptr.alloc_id.0 * 997 + i as u64 * 7193) as usize % ub_chars.len();
                write!(&mut s, "<mark>{}{}</mark>", ub_chars[c1], ub_chars[c2]).unwrap();
            }
            i += 1;
        }
    }
    s
}
