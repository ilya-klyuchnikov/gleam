use super::*;
use crate::{
    ast::*,
    line_numbers::LineNumbers,
    pretty::*,
    type_::{FieldMap, ModuleValueConstructor, ValueConstructor, ValueConstructorVariant},
};

static RECORD_KEY: &str = "type";

#[derive(Debug)]
pub struct Generator<'module> {
    module_name: &'module [String],
    line_numbers: &'module LineNumbers,
    function_name: &'module str,
    tail_position: bool,
    // We register whether float division is used within an expression so that
    // the module generator can output a suitable function if it is needed.
    float_division_used: &'module mut bool,
    object_equality_used: &'module mut bool,
}

impl<'module> Generator<'module> {
    pub fn new(
        module_name: &'module [String],
        line_numbers: &'module LineNumbers,
        function_name: &'module str,
        float_division_used: &'module mut bool,
        object_equality_used: &'module mut bool,
    ) -> Self {
        Self {
            module_name,
            line_numbers,
            function_name,
            tail_position: true,
            float_division_used,
            object_equality_used,
        }
    }

    pub fn expression<'a>(&mut self, expression: &'a TypedExpr) -> Output<'a> {
        let document = match expression {
            TypedExpr::String { value, .. } => Ok(string(value)),

            TypedExpr::Int { value, .. } => Ok(int(value)),
            TypedExpr::Float { value, .. } => Ok(float(value)),

            TypedExpr::List { elements, tail, .. } => {
                let inner = tail
                    .as_ref()
                    .map(|t| self.not_in_tail_position(|gen| gen.wrap_expression(t)))
                    .unwrap_or_else(|| Ok("[]".to_doc()))?;
                let renderer =
                    |element| self.not_in_tail_position(|gen| gen.wrap_expression(element));
                wrap_list(elements, inner, renderer)
            }

            TypedExpr::Tuple { elems, .. } => self.tuple(elems),
            TypedExpr::TupleIndex { tuple, index, .. } => self.tuple_index(tuple, *index),

            TypedExpr::Case { .. } => unsupported("Case"),

            TypedExpr::Call { fun, args, .. } => self.call(fun, args),
            TypedExpr::Fn { args, body, .. } => self.fun(args, body),

            TypedExpr::RecordAccess { record, label, .. } => self.record_access(record, label),
            TypedExpr::RecordUpdate { spread, args, .. } => self.record_update(spread, args),

            TypedExpr::Var {
                name, constructor, ..
            } => self.variable(name, constructor),

            TypedExpr::Sequence { expressions, .. } => self.sequence(expressions),

            TypedExpr::Assignment { value, pattern, .. } => self.let_(value, pattern),

            TypedExpr::Try { .. } => unsupported("Try"),

            TypedExpr::BinOp {
                name, left, right, ..
            } => self.bin_op(name, left, right),

            TypedExpr::Todo {
                label, location, ..
            } => Ok(self.todo(label, location)),

            TypedExpr::BitString { .. } => unsupported("Bitstring"),

            TypedExpr::Pipe { .. } => unsupported("Pipe"),

            TypedExpr::ModuleSelect {
                module_alias,
                label,
                constructor,
                ..
            } => self.module_select(module_alias, label, constructor),
        }?;
        Ok(match expression {
            TypedExpr::Sequence { .. } | TypedExpr::Assignment { .. } => document,
            _ => match self.tail_position {
                true => docvec!["return ", document, ";"],
                _ => document,
            },
        })
    }

    fn not_in_tail_position<'a, CompileFn>(&mut self, compile: CompileFn) -> Output<'a>
    where
        CompileFn: Fn(&mut Self) -> Output<'a>,
    {
        let tail = self.tail_position;
        self.tail_position = false;
        let result = compile(self);
        self.tail_position = tail;
        result
    }

    /// Wrap an expression in an immediately involked function expression if
    /// required due to being a JS statement
    fn wrap_expression<'a>(&mut self, expression: &'a TypedExpr) -> Output<'a> {
        match expression {
            TypedExpr::Sequence { .. } | TypedExpr::Assignment { .. } => {
                self.immediately_involked_function_expression(expression)
            }
            _ => self.expression(expression),
        }
    }

    /// Wrap an expression in an immediately involked function expression
    fn immediately_involked_function_expression<'a>(
        &mut self,
        expression: &'a TypedExpr,
    ) -> Output<'a> {
        let tail = self.tail_position;
        self.tail_position = true;
        let result = self.expression(expression);
        self.tail_position = tail;
        Ok(docvec!(
            docvec!("(() => {", break_("", " "), result?)
                .nest(INDENT)
                .group(),
            break_("", " "),
            "})()",
        ))
    }

    fn variable<'a>(&mut self, name: &'a str, constructor: &'a ValueConstructor) -> Output<'a> {
        match &constructor.variant {
            ValueConstructorVariant::Record { name, .. }
                if constructor.type_.is_bool() && name == "True" =>
            {
                Ok("true".to_doc())
            }
            ValueConstructorVariant::Record { name, .. }
                if constructor.type_.is_bool() && name == "False" =>
            {
                Ok("false".to_doc())
            }
            ValueConstructorVariant::Record { .. } if constructor.type_.is_nil() => {
                Ok("undefined".to_doc())
            }
            ValueConstructorVariant::Record { name, arity: 0, .. } => {
                let record_type = name.to_doc().surround("\"", "\"");
                let record_head = (RECORD_KEY.to_doc(), Some(record_type));
                Ok(wrap_object(std::iter::once(record_head)))
            }
            ValueConstructorVariant::Record {
                name,
                arity,
                field_map,
                ..
            } => {
                let vars = (0..*arity)
                    .into_iter()
                    .map(|i| Document::String(format!("var{}", i)));

                let body = docvec![
                    "return ",
                    construct_record(name, *arity, field_map, vars.clone()),
                    ";"
                ];

                Ok(docvec!(
                    docvec!(wrap_args(vars), " => {", break_("", " "), body,)
                        .nest(INDENT)
                        .append(break_("", " "))
                        .group(),
                    "}",
                ))
            }
            ValueConstructorVariant::LocalVariable => Ok(name.to_doc()),
            ValueConstructorVariant::ModuleFn { .. } => Ok(name.to_doc()),
            _ => unsupported("Referencing variables"),
        }
    }

    fn sequence<'a>(&mut self, expressions: &'a [TypedExpr]) -> Output<'a> {
        let count = expressions.len();
        let mut documents = Vec::with_capacity(count * 3);
        documents.push(force_break());
        for (i, expression) in expressions.iter().enumerate() {
            if i + 1 < count {
                documents.push(self.not_in_tail_position(|gen| gen.expression(expression))?);
                documents.push(";".to_doc());
                documents.push(line());
            } else {
                documents.push(self.expression(expression)?);
            }
        }
        Ok(documents.to_doc())
    }

    fn let_<'a>(&mut self, _value: &'a TypedExpr, pattern: &'a TypedPattern) -> Output<'a> {
        let mut checks = vec![];
        // let mut assignments = vec![];
        match pattern {
            Pattern::String { value, .. } => {
                checks.push(equality(string(value)));
                Ok(())
            }
            Pattern::Int { value, .. } => {
                checks.push(equality(int(value)));
                Ok(())
            }
            Pattern::Float { value, .. } => {
                checks.push(equality(float(value)));
                Ok(())
            }
            // TODO test
            Pattern::Var { .. } => Ok(()),
            // TODO test
            Pattern::Discard { .. } => Ok(()),
            Pattern::Assign { .. } => unimplemented!("as syntax not supported in JS backend"),

            Pattern::List { .. } => unimplemented!("List matching not supported in JS backend"),
            Pattern::Tuple { .. } => unimplemented!("Tuple matching not supported in JS backend"),
            Pattern::Constructor { .. } => {
                unimplemented!("Custom type matching not supported in JS backend")
            }

            Pattern::VarUsage { .. } | Pattern::BitString { .. } => {
                unimplemented!("BitString not supported")
            }
        }?;
        let check_line = match checks.is_empty() {
            true => unimplemented!("todo"),
            false => docvec![
                "if (!(",
                concat(Itertools::intersperse(checks.into_iter(), " && ".to_doc())),
                ")) throw new Error(\"Bad match\")"
            ],
        };
        Ok(check_line)
    }

    fn tuple<'a>(&mut self, elements: &'a [TypedExpr]) -> Output<'a> {
        self.not_in_tail_position(|gen| {
            array(elements.iter().map(|element| gen.wrap_expression(element)))
        })
    }

    fn call<'a>(&mut self, fun: &'a TypedExpr, arguments: &'a [CallArg<TypedExpr>]) -> Output<'a> {
        match fun {
            // Short circuit creation of a record so the rendered JS creates the record inline.
            TypedExpr::Var {
                constructor:
                    ValueConstructor {
                        variant:
                            ValueConstructorVariant::Record {
                                name,
                                arity,
                                field_map,
                                ..
                            },
                        ..
                    },
                ..
            } => {
                let tail = self.tail_position;
                self.tail_position = false;
                let arguments = arguments
                    .iter()
                    .map(|element| self.wrap_expression(&element.value))
                    .collect::<Result<Vec<_>, _>>()?;
                self.tail_position = tail;

                Ok(construct_record(
                    name,
                    *arity,
                    field_map,
                    arguments.into_iter(),
                ))
            }
            _ => {
                let fun = self.not_in_tail_position(|gen| gen.expression(fun))?;
                let arguments = self.not_in_tail_position(|gen| {
                    call_arguments(
                        arguments
                            .iter()
                            .map(|element| gen.wrap_expression(&element.value)),
                    )
                })?;
                Ok(docvec![fun, arguments])
            }
        }
    }

    fn fun<'a>(&mut self, arguments: &'a [TypedArg], body: &'a TypedExpr) -> Output<'a> {
        let tail = self.tail_position;
        self.tail_position = true;
        let result = self.expression(body);
        self.tail_position = tail;
        Ok(docvec!(
            docvec!(fun_args(arguments), " => {", break_("", " "), result?,)
                .nest(INDENT)
                .append(break_("", " "))
                .group(),
            "}",
        ))
    }

    fn record_access<'a>(&mut self, record: &'a TypedExpr, label: &'a str) -> Output<'a> {
        self.not_in_tail_position(|gen| {
            let record = gen.wrap_expression(record)?;
            Ok(docvec![record, ".", label])
        })
    }

    fn record_update<'a>(
        &mut self,
        spread: &'a TypedExpr,
        updates: &'a [TypedRecordUpdateArg],
    ) -> Output<'a> {
        self.not_in_tail_position(|gen| {
            let spread = gen.wrap_expression(spread)?;
            let updates: Vec<(Document<'a>, Option<Document<'a>>)> = updates
                .iter()
                .map(|TypedRecordUpdateArg { label, value, .. }| {
                    Ok((label.to_doc(), Some(gen.wrap_expression(value)?)))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let assign_args = vec!["{}".to_doc(), spread, wrap_object(updates.into_iter())];
            Ok(docvec!["Object.assign", wrap_args(assign_args.into_iter())])
        })
    }

    fn tuple_index<'a>(&mut self, tuple: &'a TypedExpr, index: u64) -> Output<'a> {
        self.not_in_tail_position(|gen| {
            let tuple = gen.wrap_expression(tuple)?;
            Ok(docvec![tuple, Document::String(format!("[{}]", index))])
        })
    }

    // TODO: handle precedence rules
    fn bin_op<'a>(
        &mut self,
        name: &'a BinOp,
        left: &'a TypedExpr,
        right: &'a TypedExpr,
    ) -> Output<'a> {
        let left = self.not_in_tail_position(|gen| gen.expression(left))?;
        let right = self.not_in_tail_position(|gen| gen.expression(right))?;
        Ok(match name {
            BinOp::And => self.print_bin_op(left, right, "&&"),
            BinOp::Or => self.print_bin_op(left, right, "||"),
            BinOp::LtInt | BinOp::LtFloat => self.print_bin_op(left, right, "<"),
            BinOp::LtEqInt | BinOp::LtEqFloat => self.print_bin_op(left, right, "<="),
            BinOp::Eq => {
                use std::iter::once;
                *self.object_equality_used = true;
                docvec!("$deepEqual", wrap_args(once(left).chain(once(right))))
            }
            BinOp::NotEq => {
                use std::iter::once;
                *self.object_equality_used = true;
                docvec!("!$deepEqual", wrap_args(once(left).chain(once(right))))
            }
            BinOp::GtInt | BinOp::GtFloat => self.print_bin_op(left, right, ">"),
            BinOp::GtEqInt | BinOp::GtEqFloat => self.print_bin_op(left, right, ">="),
            BinOp::AddInt | BinOp::AddFloat => self.print_bin_op(left, right, "+"),
            BinOp::SubInt | BinOp::SubFloat => self.print_bin_op(left, right, "-"),
            BinOp::MultInt | BinOp::MultFloat => self.print_bin_op(left, right, "*"),
            BinOp::DivInt => self.print_bin_op(left, right, "/").append(" | 0"),
            BinOp::ModuloInt => self.print_bin_op(left, right, "%"),
            BinOp::DivFloat => {
                use std::iter::once;
                *self.float_division_used = true;
                docvec!("$divide", wrap_args(once(left).chain(once(right))))
            }
        })
    }

    fn print_bin_op<'a>(
        &mut self,
        left: Document<'a>,
        right: Document<'a>,
        op: &'a str,
    ) -> Document<'a> {
        left.append(" ")
            .append(op.to_doc())
            .append(" ")
            .append(right)
    }

    fn todo<'a>(&mut self, message: &'a Option<String>, location: &'a SrcSpan) -> Document<'a> {
        self.tail_position = false;
        let gleam_error = "todo";
        let message = message
            .as_ref()
            .map(|s| s.as_str())
            .unwrap_or_else(|| "This has not yet been implemented");
        let module_name = Document::String(self.module_name.join("_")); // TODO: This isn't right
        let line = self.line_numbers.line_number(location.start);

        docvec![
            "throw Object.assign",
            wrap_args(
                vec![
                    docvec!["new Error", wrap_args(std::iter::once(string(message)))],
                    wrap_object(
                        vec![
                            ("gleam_error".to_doc(), Some(string(gleam_error))),
                            ("module".to_doc(), Some(module_name.surround("\"", "\""))),
                            (
                                "function".to_doc(),
                                Some(
                                    // TODO switch to use `string(self.function_name)`
                                    // This will require resolving the difference in lifetimes 'module and 'a.
                                    Document::String(self.function_name.to_string())
                                        .surround("\"", "\"")
                                )
                            ),
                            ("line".to_doc(), Some(line.to_doc())),
                        ]
                        .into_iter()
                    )
                ]
                .into_iter()
            )
        ]
    }

    fn module_select<'a>(
        &mut self,
        module: &'a str,
        label: &'a str,
        constructor: &'a ModuleValueConstructor,
    ) -> Output<'a> {
        match constructor {
            ModuleValueConstructor::Fn | ModuleValueConstructor::Constant { .. } => {
                Ok(docvec![module, ".", label])
            }
            _ => unsupported("Module function call"),
        }
    }
}

fn equality<'a>(to_match: Document<'a>) -> Document<'a> {
    docvec!["gleam$tmp", " === ", to_match]
}

fn int(value: &str) -> Document<'_> {
    value.to_doc()
}

fn float(value: &str) -> Document<'_> {
    value.to_doc()
}
pub fn constant_expression(expression: &'_ TypedConstant) -> Output<'_> {
    match expression {
        Constant::Int { value, .. } => Ok(int(value)),
        Constant::Float { value, .. } => Ok(float(value)),
        Constant::String { value, .. } => Ok(string(value.as_str())),
        Constant::Tuple { elements, .. } => array(elements.iter().map(|e| constant_expression(e))),
        Constant::List { elements, .. } => wrap_list(elements, "[]".to_doc(), constant_expression),
        Constant::Record { typ, name, .. } if typ.is_bool() && name == "True" => {
            Ok("true".to_doc())
        }
        Constant::Record { typ, name, .. } if typ.is_bool() && name == "False" => {
            Ok("false".to_doc())
        }
        Constant::Record { typ, .. } if typ.is_nil() => Ok("undefined".to_doc()),
        Constant::Record {
            tag,
            args,
            field_map,
            ..
        } => {
            let field_values: Result<Vec<_>, _> = args
                .iter()
                .map(|arg| constant_expression(&arg.value))
                .collect();
            Ok(construct_record(
                tag,
                args.len(),
                field_map,
                field_values?.into_iter(),
            ))
        }
        Constant::BitString { .. } => unsupported("BitString as constant"),
    }
}

fn string(value: &str) -> Document<'_> {
    value.to_doc().surround("\"", "\"")
}

fn array<'a, Elements: Iterator<Item = Output<'a>>>(elements: Elements) -> Output<'a> {
    let elements = Itertools::intersperse(elements, Ok(break_(",", ", ")))
        .collect::<Result<Vec<_>, _>>()?
        .to_doc();
    Ok(docvec![
        "[",
        docvec![break_("", ""), elements].nest(INDENT),
        break_(",", ""),
        "]"
    ]
    .group())
}

fn wrap_list<'a, E>(
    elements: &'a [E],
    inner: Document<'a>,
    mut renderer: impl FnMut(&'a E) -> Output<'a>,
) -> Output<'a> {
    match elements.split_last() {
        Some((element, rest)) => {
            let inner = docvec![
                "[",
                docvec![renderer(element)?, break_(",", ", "), inner,]
                    .nest(INDENT)
                    .group(),
                "]",
            ];
            wrap_list(rest, inner, renderer)
        }
        None => Ok(inner),
    }
}

fn call_arguments<'a, Elements: Iterator<Item = Output<'a>>>(elements: Elements) -> Output<'a> {
    let elements = Itertools::intersperse(elements, Ok(break_(",", ", ")))
        .collect::<Result<Vec<_>, _>>()?
        .to_doc();
    Ok(docvec![
        "(",
        docvec![break_("", ""), elements].nest(INDENT),
        break_(",", ""),
        ")"
    ]
    .group())
}

fn construct_record<'a>(
    name: &'a str,
    arity: usize,
    field_map: &'a Option<FieldMap>,
    values: impl Iterator<Item = Document<'a>>,
) -> Document<'a> {
    let field_names: Vec<Document<'_>> = match field_map {
        Some(FieldMap { fields, .. }) => fields
            .iter()
            .sorted_by_key(|(_, &v)| v)
            .map(|x| x.0.as_str().to_doc())
            .collect(),
        None => (0..arity)
            .into_iter()
            .map(|i| Document::String(format!("{}", i)))
            .collect(),
    };

    let record_head = (
        RECORD_KEY.to_doc(),
        Some(name.to_doc().surround("\"", "\"")),
    );

    let record_values = field_names
        .into_iter()
        .zip(values)
        .map(|(name, value)| (name, Some(value)));

    wrap_object(std::iter::once(record_head).chain(record_values))
}
