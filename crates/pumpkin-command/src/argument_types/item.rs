use pumpkin_data::data_component::DataComponent;
use pumpkin_data::item::Item;
use pumpkin_data::item_stack::ItemStack;
use pumpkin_data::translation;
use pumpkin_util::text::TextComponent;

use crate::argument_types::argument_type::{ArgumentType, JavaClientArgumentType};
use crate::context::command_context::CommandContext;
use crate::errors::command_syntax_error::CommandSyntaxError;
use crate::errors::error_types::{CommandErrorType, READER_EXPECTED_SYMBOL};
use crate::snbt::SnbtParser;
use crate::string_reader::StringReader;
use crate::suggestion::suggestions::{Suggestions, SuggestionsBuilder};

pub const ERROR_UNKNOWN_ITEM: CommandErrorType<1> = CommandErrorType::new(
    translation::java::ARGUMENT_ITEM_ID_INVALID,
    translation::java::ARGUMENT_ITEM_ID_INVALID,
);
pub const ERROR_COMPONENT_EXPECTED: CommandErrorType<0> = CommandErrorType::new(
    translation::java::ARGUMENTS_ITEM_COMPONENT_EXPECTED,
    translation::java::ARGUMENTS_ITEM_COMPONENT_EXPECTED,
);
pub const ERROR_COMPONENT_MALFORMED: CommandErrorType<2> = CommandErrorType::new(
    translation::java::ARGUMENTS_ITEM_COMPONENT_MALFORMED,
    translation::java::ARGUMENTS_ITEM_COMPONENT_MALFORMED,
);
pub const ERROR_COMPONENT_REPEATED: CommandErrorType<1> = CommandErrorType::new(
    translation::java::ARGUMENTS_ITEM_COMPONENT_REPEATED,
    translation::java::ARGUMENTS_ITEM_COMPONENT_REPEATED,
);
pub const ERROR_COMPONENT_UNKNOWN: CommandErrorType<1> = CommandErrorType::new(
    translation::java::ARGUMENTS_ITEM_COMPONENT_UNKNOWN,
    translation::java::ARGUMENTS_ITEM_COMPONENT_UNKNOWN,
);

#[derive(Clone, Copy)]
pub struct ItemStackArgumentType;

fn expected_component_symbol(reader: &StringReader, symbol: &'static str) -> CommandSyntaxError {
    READER_EXPECTED_SYMBOL.create(reader, TextComponent::text(symbol))
}

fn canonical_component_name(name: String) -> String {
    if name.contains(':') {
        name
    } else {
        format!("minecraft:{name}")
    }
}

impl<S: crate::source::CommandSource> ArgumentType<S> for ItemStackArgumentType {
    type Item = ItemStack;

    fn parse(&self, reader: &mut StringReader) -> Result<Self::Item, CommandSyntaxError> {
        let start = reader.cursor();
        while let Some(c) = reader.peek() {
            if c.is_alphanumeric() || c == '_' || c == ':' || c == '/' || c == '.' || c == '-' {
                reader.skip();
            } else {
                break;
            }
        }
        let raw_id = &reader.string()[start..reader.cursor()];
        let item = Item::from_registry_key(raw_id).ok_or_else(|| {
            let full_name = if raw_id.contains(':') {
                raw_id.to_string()
            } else {
                format!("minecraft:{raw_id}")
            };
            ERROR_UNKNOWN_ITEM.create(reader, TextComponent::text(full_name))
        })?;

        let mut stack = ItemStack::new(1, item);

        // Optional components [...]
        if reader.peek() == Some('[') {
            reader.skip();
            let mut patch = Vec::new();
            loop {
                reader.skip_whitespace();
                if reader.peek() == Some(']') {
                    reader.skip();
                    break;
                }
                if !reader.can_read_char() {
                    return Err(ERROR_COMPONENT_EXPECTED.create(reader));
                }

                let removed = reader.peek() == Some('!');
                if removed {
                    reader.skip();
                }
                let name_start = reader.cursor();
                while let Some(c) = reader.peek() {
                    if c.is_alphanumeric() || c == '_' || c == ':' || c == '.' || c == '-' {
                        reader.skip();
                    } else {
                        break;
                    }
                }
                let key_str = reader.string()[name_start..reader.cursor()].to_owned();
                let Some(data_comp) = DataComponent::try_from_name(&key_str) else {
                    reader.set_cursor(name_start);
                    return Err(ERROR_COMPONENT_UNKNOWN.create(
                        reader,
                        TextComponent::text(canonical_component_name(key_str)),
                    ));
                };
                if patch.iter().any(|(id, _)| *id == data_comp) {
                    reader.set_cursor(name_start);
                    return Err(ERROR_COMPONENT_REPEATED
                        .create_without_context(TextComponent::text(data_comp.to_name())));
                }

                reader.skip_whitespace();
                if removed {
                    if reader.peek() == Some('=') {
                        return Err(expected_component_symbol(reader, "]"));
                    }
                    patch.push((data_comp, None));
                } else {
                    if reader.peek() != Some('=') {
                        return Err(expected_component_symbol(reader, "="));
                    }
                    reader.skip();
                    reader.skip_whitespace();
                    let value_start = reader.cursor();
                    let nbt_tag = SnbtParser::parse_for_commands(reader)?;
                    let Some(comp_impl) =
                        pumpkin_data::data_component_impl::read_data(data_comp, &nbt_tag)
                    else {
                        reader.set_cursor(value_start);
                        return Err(ERROR_COMPONENT_MALFORMED.create_args_slice(
                            reader,
                            &[
                                TextComponent::text(data_comp.to_name()),
                                TextComponent::text("invalid component value"),
                            ],
                        ));
                    };
                    patch.push((data_comp, Some(comp_impl)));
                }

                reader.skip_whitespace();
                match reader.peek() {
                    Some(',') => {
                        reader.skip();
                    }
                    Some(']') => {
                        reader.skip();
                        break;
                    }
                    _ => return Err(expected_component_symbol(reader, "]")),
                }
            }
            if !patch.is_empty() {
                stack.patch = patch;
            }
        }

        Ok(stack)
    }

    fn client_side_parser(&'_ self) -> JavaClientArgumentType {
        JavaClientArgumentType::ItemStack
    }

    fn list_suggestions(
        &self,
        _context: &CommandContext<S>,
        builder: SuggestionsBuilder,
    ) -> Suggestions {
        builder.build()
    }
}

impl ItemStackArgumentType {
    pub fn get<S: crate::source::CommandSource>(
        context: &CommandContext<S>,
        name: &str,
    ) -> Result<ItemStack, CommandSyntaxError> {
        context.get_argument::<ItemStack>(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::{ERROR_COMPONENT_REPEATED, ERROR_COMPONENT_UNKNOWN, ItemStackArgumentType};
    use crate::argument_types::argument_type::ArgumentType;
    use crate::errors::error_types::READER_EXPECTED_SYMBOL;
    use crate::string_reader::StringReader;
    use pumpkin_data::data_component::DataComponent;

    fn parse(
        input: &str,
    ) -> Result<
        pumpkin_data::item_stack::ItemStack,
        crate::errors::command_syntax_error::CommandSyntaxError,
    > {
        <ItemStackArgumentType as ArgumentType<()>>::parse(
            &ItemStackArgumentType,
            &mut StringReader::new(input),
        )
    }

    #[test]
    fn duplicate_command_components_reject_values_aliases_and_removals() {
        for input in [
            "minecraft:iron_sword[damage=1,damage=2]",
            "iron_sword[minecraft:damage=1,damage=2]",
            "iron_sword[!damage,damage=1]",
            "iron_sword[damage=1,!minecraft:damage]",
            "iron_sword[!damage,!minecraft:damage]",
        ] {
            let Err(error) = parse(input) else {
                panic!("{input}");
            };
            assert!(error.is(&ERROR_COMPONENT_REPEATED), "{input}");
            assert!(error.context.is_none(), "{input}");
        }
    }

    #[test]
    fn unknown_command_component_reports_name_start() {
        let input = "iron_sword[!missing]";
        let error = match parse(input) {
            Ok(_) => panic!("{input}"),
            Err(error) => error,
        };
        assert!(error.is(&ERROR_COMPONENT_UNKNOWN));
        assert_eq!(error.context.unwrap().cursor, "iron_sword[!".len());
    }

    #[test]
    fn command_component_remove_and_distinct_values_parse() {
        let removed = parse("iron_sword[!damage]").expect("removed component");
        assert_eq!(removed.patch.len(), 1);
        assert!(removed.patch[0].0 == DataComponent::Damage);
        assert!(removed.patch[0].1.is_none());

        let distinct = parse("iron_sword[damage=1,unbreakable={}]").expect("distinct components");
        assert_eq!(distinct.patch.len(), 2);
    }

    #[test]
    fn command_component_invalid_shapes_match_vanilla_errors_and_cursors() {
        for (input, cursor) in [
            ("iron_sword[!damage=1]", "iron_sword[!damage".len()),
            ("iron_sword[damage]", "iron_sword[damage".len()),
        ] {
            let error = match parse(input) {
                Ok(_) => panic!("{input}"),
                Err(error) => error,
            };
            assert!(error.is(&READER_EXPECTED_SYMBOL), "{input}");
            assert_eq!(error.context.as_ref().unwrap().cursor, cursor, "{input}");
        }
    }

    #[test]
    fn command_components_accept_whitespace_nested_snbt_and_quoted_text() {
        let stack = parse(
            r#"iron_sword[ custom_data = {foo:[1,2,{text:"a,b"}]} , custom_name = {text:"quoted \"text\" and ]"} ]"#,
        )
        .expect("valid component syntax");
        assert_eq!(stack.patch.len(), 2);
    }

    #[test]
    fn malformed_component_value_reports_the_value_start() {
        let input = "iron_sword[damage={}]";
        let error = match parse(input) {
            Ok(_) => panic!("damage requires an integer"),
            Err(error) => error,
        };
        assert!(
            error
                .context
                .is_some_and(|context| { context.cursor == input.find('{').expect("value start") })
        );
    }
}
