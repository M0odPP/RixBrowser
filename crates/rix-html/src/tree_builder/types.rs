//! Internal state types for the WHATWG HTML5 tree builder.

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::Tag;

#[derive(PartialEq, Eq, Copy, Clone, Debug)]
pub enum InsertionMode {
    Initial,
    BeforeHtml,
    BeforeHead,
    InHead,
    InHeadNoscript,
    AfterHead,
    InBody,
    Text,
    InTable,
    InTableText,
    InCaption,
    InColumnGroup,
    InTableBody,
    InRow,
    InCell,
    InTemplate,
    AfterBody,
    InFrameset,
    AfterFrameset,
    AfterAfterBody,
    AfterAfterFrameset,
}

#[derive(PartialEq, Eq, Copy, Clone, Debug)]
pub enum SplitStatus {
    NotSplit,
    Whitespace,
    NotWhitespace,
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub enum Token {
    Tag(Tag),
    Comment(StrTendril),
    Characters(SplitStatus, StrTendril),
    NullCharacter,
    Eof,
}

pub enum ProcessResult<Handle> {
    Done,
    DoneAckSelfClosing,
    SplitWhitespace(StrTendril),
    Reprocess(InsertionMode, Token),
    Script(Handle),
    ToPlaintext,
    ToRawData(RawKind),
    EncodingIndicator(StrTendril),
}

pub enum FormatEntry<Handle> {
    Element(Handle, Tag),
    Marker,
}

pub enum InsertionPoint<Handle> {
    LastChild(Handle),
    TableFosterParenting {
        element: Handle,
        prev_element: Handle,
    },
}

pub enum PushFlag {
    Push,
    NoPush,
}
