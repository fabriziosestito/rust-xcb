use super::r#struct::{make_field, ResolvedFields};
use super::{CodeGen, Event, Field, WireSz};
use crate::cg::r#struct::RANDR_SUBCODES;
use crate::cg::{self, Expr, StructStyle};
use crate::cg::{util, QualifiedRsTyp};
use crate::ir;

use std::io::{self, Write};

impl CodeGen {
    pub(super) fn resolve_event(
        &mut self,
        name: String,
        number: i32,
        mut fields: Vec<ir::Field>,
        is_xge: bool,
        no_seq_number: bool,
        doc: Option<ir::Doc>,
    ) {
        let doc = self.resolve_doc(doc);

        let (fields, _must_pack) = {
            let mut ff = vec![make_field("response_type".into(), "CARD8".into())];
            let mut must_pack = false;

            let mut sz = 1; // response_type size

            if is_xge {
                ff.push(make_field("extension".into(), "CARD8".into()));
                ff.push(make_field("sequence".into(), "CARD16".into()));
                ff.push(make_field("length".into(), "CARD32".into()));
                ff.push(make_field("event_type".into(), "CARD16".into()));
                sz += 9;
            } else if !no_seq_number {
                ff.push(fields.remove(0));
                ff.push(make_field("sequence".into(), "CARD16".into()));
            }

            for f in fields.into_iter() {
                if is_xge {
                    let fsz = self.ir_field_sizeof(&f);
                    ff.push(f);
                    if sz < 32 {
                        sz += fsz
                            .fixed_length()
                            .expect("can't compute ffi full_sequence position");
                        if sz == 32 {
                            ff.push(make_field("full_sequence".into(), "CARD32".into()));
                        }
                    } else if let Expr::Value(fsz) = fsz {
                        if fsz == 8 {
                            must_pack = true;
                        }
                    }
                } else {
                    ff.push(f);
                }
            }
            (ff, must_pack)
        };

        let variant = cg::rust_type_name(&name);
        let rs_typ = variant.clone() + "Event";

        let ResolvedFields {
            mut fields,
            wire_sz,
            ..
        } = self.resolv_struct_fields(&rs_typ, "", &fields, doc.as_ref());

        self.rs_typs_need_count.insert(rs_typ.clone(), 1);

        for f in &mut fields {
            if let Field::Field { ref mut name, .. } = f {
                if name == "new" {
                    *name = "new_".to_string();
                }
            }
        }

        self.events.push(Event {
            rs_typ,
            variant,
            number,
            fields,
            copy_from_rs_typ: None,
            wire_sz,
            doc,
            is_xge,
        });
    }

    pub(super) fn resolve_eventcopy(&mut self, name: String, number: i32, r#ref: String) {
        let variant = cg::rust_type_name(&name);
        let rs_typ = variant.clone() + "Event";
        let (ref_module, ref_variant) = self.extract_module(&r#ref);
        let ref_variant = cg::rust_variant_name(ref_variant);

        let mut implicit_module = None;

        let event = match &ref_module {
            Some(module) => {
                let di = self
                    .depinfo
                    .iter()
                    .find(|di| di.xcb_mod == *module)
                    .unwrap_or_else(|| panic!("could not find {} dependency", module));
                di.events
                    .iter()
                    .find(|e| e.variant == ref_variant)
                    .unwrap_or_else(|| panic!("could not find event {}::{}", module, ref_variant))
            }
            None => self
                .events
                .iter()
                .find(|e| e.variant == ref_variant)
                .or_else(|| {
                    for di in &self.depinfo {
                        for ev in &di.events {
                            if ev.variant == ref_variant {
                                implicit_module = Some(di.xcb_mod.clone());
                                return Some(ev);
                            }
                        }
                    }
                    None
                })
                .unwrap_or_else(|| {
                    panic!(
                        "{}: cannot find error {} referenced by {}",
                        self.xcb_mod, r#ref, name
                    )
                }),
        }
        .clone();

        self.events.push(Event {
            rs_typ,
            variant,
            number,
            copy_from_rs_typ: Some(event.rs_typ),
            fields: event.fields,
            wire_sz: event.wire_sz,
            is_xge: event.is_xge,
            doc: event.doc,
        });
    }

    fn ir_field_sizeof(&self, f: &ir::Field) -> Expr {
        match f {
            ir::Field::Field { typ, .. } => self.typ_wire_sz(typ),
            ir::Field::List {
                typ,
                len_expr: ir::Expr::Value(len),
                ..
            } => {
                if let Expr::Value(wire_sz) = self.typ_wire_sz(typ) {
                    Expr::Value(*len * wire_sz)
                } else {
                    Expr::Unknown("variable list".into())
                }
            }
            ir::Field::List { .. } | ir::Field::ListNoLen { .. } => {
                Expr::Unknown("variable list".into())
            }
            ir::Field::Pad(sz) => Expr::Value(*sz),
            _ => unreachable!("{:#?}", f),
        }
    }

    fn typ_wire_sz(&self, typ: &str) -> Expr {
        let (module, typ) = util::extract_module(typ);
        let typinfo = self.find_typinfo(module, typ);
        typinfo.wire_sz()
    }

    pub(crate) fn emit_events<O: Write>(&self, out: &mut O) -> io::Result<()> {
        if self.events.is_empty() {
            return Ok(());
        }

        for event in &self.events {
            if event.is_xge && self.xcb_mod == "xproto" {
                // event GeGeneric is not to be emitted because it is meant
                // to refer to special extension events (xge events)
                // rust-xcb handles event in much better way than the C bindings
                continue;
            }

            if let Some(copy_from_rs_typ) = &event.copy_from_rs_typ {
                writeln!(out)?;
                if let Some(doc) = &event.doc {
                    doc.emit(out, 0)?;
                } else {
                    writeln!(out, "/// The `{}` event.", event.rs_typ)?;
                }
                writeln!(out, "pub type {} = {};", event.rs_typ, copy_from_rs_typ)?;
                continue;
            }

            // Event are struct holding a pointer.
            // They own the data pointed to that must be freed during drop.

            let (trait_impl, raw_typ) = if event.is_xge {
                ("base::GeEvent", "xcb_ge_generic_event_t")
            } else {
                ("base::BaseEvent", "xcb_generic_event_t")
            };

            writeln!(out)?;
            if let Some(doc) = &event.doc {
                doc.emit(out, 0)?;
            } else {
                writeln!(out, "/// The `{}` event.", event.rs_typ)?;
            }
            writeln!(out, "pub struct {} {{", event.rs_typ)?;
            writeln!(out, "    raw: *mut {},", raw_typ)?;
            writeln!(out, "}}")?;

            writeln!(out)?;
            writeln!(out, "impl {} for {} {{", trait_impl, event.rs_typ)?;
            if event.is_xge {
                writeln!(
                    out,
                    "    const EXTENSION: ext::Extension = ext::Extension::{};",
                    self.ext_info.as_ref().unwrap().rs_name
                )?;
                writeln!(out)?;
            } else if let Some(ext_info) = self.ext_info.as_ref() {
                writeln!(
                        out,
                        "    const EXTENSION: std::option::Option<ext::Extension> = Some(ext::Extension::{});",
                        ext_info.rs_name
                    )?;
            } else {
                writeln!(
                    out,
                    "    const EXTENSION: std::option::Option<ext::Extension> = None;"
                )?;
            }
            writeln!(out, "    const NUMBER: u32 = {};", event.number)?;
            writeln!(out)?;
            writeln!(
                out,
                "    unsafe fn from_raw(raw: *mut {}) -> Self {{ {} {{ raw }} }}",
                raw_typ, event.rs_typ
            )?;
            writeln!(out)?;
            writeln!(out, "    unsafe fn into_raw(self) -> *mut {} {{", raw_typ)?;
            writeln!(out, "        let raw = self.raw;")?;
            writeln!(out, "        std::mem::forget(self);")?;
            writeln!(out, "        raw")?;
            writeln!(out, "    }}")?;
            writeln!(out)?;
            writeln!(out, "    fn as_raw(&self) -> *mut {} {{", raw_typ)?;
            writeln!(out, "        self.raw")?;
            writeln!(out, "    }}")?;

            let len_expr = if event.is_xge {
                "self.length() as usize"
            } else {
                "32"
            };
            writeln!(out)?;
            writeln!(out, "    fn as_slice(&self) -> &[u8] {{")?;
            writeln!(out, "        unsafe {{")?;
            writeln!(
                out,
                "            std::slice::from_raw_parts(self.raw as *const u8, {})",
                len_expr
            )?;
            writeln!(out, "        }}")?;
            writeln!(out, "    }}")?;

            writeln!(out, "}}")?;

            writeln!(out)?;
            writeln!(out, "impl {} {{", event.rs_typ)?;

            if !event.is_xge {
                // we enable contruction of classic events to pass to SendEvent request
                self.emit_event_new(out, event)?;
                writeln!(out)?;
            }

            writeln!(
                out,
                "    fn wire_ptr(&self) -> *const u8 {{ self.raw as *const u8 }}"
            )?;
            self.emit_struct_accessors(out, &event.rs_typ, &event.fields)?;
            writeln!(out, "}}")?;

            self.emit_debug_impl(out, &event.rs_typ, &event.fields)?;

            writeln!(out)?;
            writeln!(out, "impl Drop for {} {{", event.rs_typ)?;
            writeln!(out, "    fn drop(&mut self) {{")?;
            writeln!(out, "        unsafe {{ libc::free(self.raw as *mut _); }}")?;
            writeln!(out, "    }}")?;
            writeln!(out, "}}")?;
        }

        writeln!(out)?;
        if let Some(ext_info) = &self.ext_info {
            writeln!(
                out,
                "/// Unified event type for the {} extension",
                ext_info.rs_name
            )?;
        } else {
            writeln!(out, "/// Unified event type for the X core protocol")?;
        }
        writeln!(out, "#[derive(Debug)]")?;
        writeln!(out, "pub enum Event {{")?;
        for event in &self.events {
            if event.is_xge && self.xcb_mod == "xproto" {
                // same comment as above
                continue;
            }
            writeln!(out, "    {}({}),", event.variant, event.rs_typ)?;
        }
        writeln!(out, "}}")?;

        let has_xge = self.events.iter().any(|ev| ev.is_xge);
        let has_non_xge = !self.events.iter().all(|ev| ev.is_xge);
        let _last_event = self.events.iter().map(|ev| ev.number).max().unwrap();

        if has_non_xge {
            self.emit_resolve_wire_event(out)?;
        }

        if has_xge && self.xcb_mod != "xproto" {
            // xproto GeGeneric is not to be considered as it refers to
            // an extension generic event
            self.emit_resolve_wire_ge_event(out)?;
        }

        Ok(())
    }

    fn emit_event_new<O: Write>(&self, out: &mut O, event: &Event) -> io::Result<()> {
        // only fixed size events, with size <= 32
        assert!(
            matches!(event.wire_sz, Expr::Value(sz) if sz <= 32),
            "{:#?}",
            event
        );

        let need_event_base = self.xcb_mod != "xproto";
        let fn_decl = if need_event_base {
            "new(event_base: u8,"
        } else {
            "new("
        };
        writeln!(out, "    pub fn {}", fn_decl)?;
        // emit parameters
        for f in &event.fields {
            match f {
                Field::Field { name, .. } if name == "response_type" => {}
                Field::Field { name, .. } if name == "sequence" => {}
                Field::Field { name, .. } if name == "format" => {}
                Field::Field { name, .. } if name == "sub_code" => {}
                Field::Field {
                    name,
                    module,
                    rs_typ,
                    struct_style: None | Some(StructStyle::FixBuf | StructStyle::WireLayout),
                    ..
                } => {
                    let q_rs_typ = (module, rs_typ).qualified_rs_typ();
                    writeln!(out, "        {}: {},", name, q_rs_typ)?;
                }
                Field::List {
                    name,
                    module,
                    rs_typ,
                    len_expr: Expr::Value(len),
                    struct_style: None | Some(StructStyle::FixBuf | StructStyle::WireLayout),
                    ..
                } => {
                    let q_rs_typ = (module, rs_typ).qualified_rs_typ();
                    writeln!(out, "        {}: [{}; {}],", name, q_rs_typ, len)?;
                }
                Field::List {
                    name,
                    module,
                    rs_typ,
                    struct_style: None | Some(StructStyle::FixBuf | StructStyle::WireLayout),
                    ..
                } => {
                    let q_rs_typ = (module, rs_typ).qualified_rs_typ();
                    writeln!(out, "        {}: &[{}],", name, q_rs_typ)?;
                }
                Field::Pad { .. } => {}
                f => unreachable!("{:#?}", f),
            }
        }
        writeln!(out, "    ) -> {} {{", event.rs_typ)?;
        writeln!(out, "{}unsafe {{", cg::ind(2))?;
        writeln!(out, "{}let ptr = libc::malloc(32) as *mut u8;", cg::ind(3),)?;
        writeln!(
            out,
            "{}let wire_buf = std::slice::from_raw_parts_mut(ptr, 32);",
            cg::ind(3)
        )?;
        writeln!(out, "{}let mut wire_off = 0usize;", cg::ind(3))?;
        if need_event_base {
            let expr = if event.number == 0 {
                "event_base".to_string()
            } else {
                format!("{}u8 + event_base", event.number)
            };
            writeln!(out, "{}let response_type = {};", cg::ind(3), expr)?;
        } else {
            writeln!(out, "{}let response_type = {}u8;", cg::ind(3), event.number)?;
        }
        writeln!(out, "{}let sequence = 0u16;", cg::ind(3))?;
        if event.rs_typ == "ClientMessageEvent" {
            writeln!(out, "{}let format: u8 = match data {{", cg::ind(3))?;
            for format in [8, 16, 32] {
                writeln!(
                    out,
                    "{}ClientMessageData::Data{}{{..}} => {},",
                    cg::ind(4),
                    format,
                    format
                )?;
            }
            writeln!(out, "{}}};", cg::ind(3))?;
        }
        if self.xcb_mod == "randr" && event.rs_typ == "NotifyEvent" {
            writeln!(out, "{}let sub_code: u8 = match u {{", cg::ind(3))?;
            for code in RANDR_SUBCODES {
                writeln!(
                    out,
                    "{}NotifyData::{}{{..}} => std::mem::transmute::<_, u32>(Notify::{}) as _,",
                    cg::ind(4),
                    code.2,
                    code.0
                )?;
            }
            writeln!(out, "{}}};", cg::ind(3))?;
        }

        writeln!(out)?;
        // emit serialization
        // for response_type we write the event number regardless of first_event
        // is it important?
        let last_is_pad = matches!(event.fields.last().unwrap(), Field::Pad { .. });
        let assignment_limit = if last_is_pad {
            event.fields.len() - 2
        } else {
            event.fields.len() - 1
        };
        for (i, f) in event.fields.iter().enumerate() {
            let assignment = if i < assignment_limit {
                "wire_off += "
            } else {
                ""
            };
            match f {
                Field::Field {
                    name,
                    rs_typ,
                    wire_sz,
                    struct_style: None | Some(StructStyle::FixBuf | StructStyle::WireLayout),
                    ..
                } => {
                    if rs_typ == "bool" {
                        if let Expr::Value(sz) = wire_sz {
                            writeln!(
                                out,
                                "{}let {}: u{} = if {} {{ 1 }} else {{ 0 }};",
                                cg::ind(3),
                                name,
                                sz * 8,
                                name,
                            )?;
                        }
                    }
                    writeln!(
                        out,
                        "{}{}{}.serialize(&mut wire_buf[wire_off ..]);",
                        cg::ind(3),
                        assignment,
                        name
                    )?;
                }
                Field::List {
                    name,
                    module,
                    rs_typ,
                    len_expr,
                    wire_sz,
                    struct_style: None | Some(StructStyle::FixBuf | StructStyle::WireLayout),
                    ..
                } => {
                    let q_rs_typ = (module, rs_typ).qualified_rs_typ();
                    writeln!(
                        out,
                        "{}std::slice::from_raw_parts_mut(ptr.add(wire_off) as *mut {}, {})",
                        cg::ind(3),
                        q_rs_typ,
                        self.build_rs_expr(len_expr, "", "", &[])
                    )?;
                    writeln!(out, "{}    .copy_from_slice(&{});", cg::ind(3), name)?;
                    if i < assignment_limit {
                        writeln!(
                            out,
                            "{}wire_off += {};",
                            cg::ind(3),
                            self.build_rs_expr(wire_sz, "", "", &[])
                        )?;
                    }
                }
                Field::Pad { .. } => {}
                f => unreachable!("{:#?}", f),
            }
        }
        writeln!(out)?;
        writeln!(
            out,
            "{}{}::from_raw(ptr as *mut xcb_generic_event_t)",
            cg::ind(3),
            event.rs_typ
        )?;
        writeln!(out, "{}}}", cg::ind(2))?;
        writeln!(out, "{}}}", cg::ind(1))?;

        Ok(())
    }

    fn emit_resolve_wire_event<O: Write>(&self, out: &mut O) -> io::Result<()> {
        writeln!(out)?;
        writeln!(out, "impl base::ResolveWireEvent for Event {{")?;
        writeln!(out, "{}unsafe fn resolve_wire_event(first_event: u8, raw: *mut xcb_generic_event_t) -> Self {{", cg::ind(1))?;
        writeln!(out, "{}debug_assert!(!raw.is_null());", cg::ind(2))?;
        writeln!(
            out,
            "{}let response_type = (*raw).response_type & 0x7F;",
            cg::ind(2)
        )?;
        writeln!(
            out,
            "{}debug_assert!(response_type != 0, \"This is not an event but an error!\");",
            cg::ind(2),
        )?;
        writeln!(
            out,
            "{}debug_assert!(response_type != XCB_GE_GENERIC, \"This is a GE_GENERIC event!\");",
            cg::ind(2),
        )?;
        if self.xcb_mod == "xkb" {
            writeln!(
                out,
                "{}assert_eq!(response_type, first_event, \"This is not an Xkb event\");",
                cg::ind(2)
            )?;
            writeln!(out, "{}let ptr = raw as *const u8;", cg::ind(2))?;
            writeln!(out, "{}let xkb_type = *(ptr.add(1));", cg::ind(2))?;
            writeln!(out, "{}match xkb_type {{", cg::ind(2))?;
        } else {
            writeln!(out, "{}match response_type - first_event {{", cg::ind(2))?;
        }
        for event in &self.events {
            if event.is_xge {
                continue;
            }

            writeln!(
                out,
                "{}{} => Event::{}({}::from_raw(raw)),",
                cg::ind(3),
                event.number,
                event.variant,
                event.rs_typ
            )?;
        }
        writeln!(out, "{}_ => unreachable!(", cg::ind(3))?;
        writeln!(
            out,
            "{}\"Could not resolve {} Event with response_type {{}} and first_event {{}}\",",
            cg::ind(4),
            self.xcb_mod
        )?;
        writeln!(out, "{}response_type, first_event", cg::ind(4))?;
        writeln!(out, "{}),", cg::ind(3))?;
        writeln!(out, "{}}}", cg::ind(2))?;
        writeln!(out, "{}}}", cg::ind(1))?;
        writeln!(out, "}}")?;

        Ok(())
    }

    fn emit_resolve_wire_ge_event<O: Write>(&self, out: &mut O) -> io::Result<()> {
        writeln!(out, "impl base::ResolveWireGeEvent for Event {{")?;
        writeln!(
            out,
            "{}unsafe fn resolve_wire_ge_event(raw: *mut xcb_ge_generic_event_t) -> Self{{",
            cg::ind(1)
        )?;
        writeln!(out, "{}debug_assert!(!raw.is_null());", cg::ind(2))?;
        writeln!(
            out,
            "{}debug_assert!(((*raw).response_type & 0x7F) == XCB_GE_GENERIC);",
            cg::ind(2)
        )?;
        writeln!(out, "{}let event_type = (*raw).event_type;", cg::ind(2))?;
        writeln!(out, "{}match event_type {{", cg::ind(2))?;
        for event in &self.events {
            if !event.is_xge {
                continue;
            }

            writeln!(
                out,
                "{}{} => Event::{}({}::from_raw(raw)),",
                cg::ind(3),
                event.number,
                event.variant,
                event.rs_typ
            )?;
        }

        writeln!(
            out,
            "{}_ => panic!(\"Could not resolve GE event for {}: {{}}\", event_type),",
            cg::ind(3),
            self.xcb_mod
        )?;
        writeln!(out, "{}}}", cg::ind(2))?;
        writeln!(out, "{}}}", cg::ind(1))?;
        writeln!(out, "}}")?;
        Ok(())
    }
}
