use core::fmt;
use std::{error::Error, ops::Range, usize};

use crate::{
    operator_precedence_parser,
    token::{Category, Keyword, Token, Tokenizer},
};

/// Is used to lookup block specific data like variables and functions.
/// The first number is the parent while the second is the own.
type BlockDepth = (u8, u8);
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Statement {
    RawNumber(u8),
    Primitive(Token),
    Variable(Token),
    Call(Token, Box<Statement>),
    Parameter(Vec<Statement>),
    Expanded(Box<Statement>, Box<Statement>), // e.g. on i++ it gets expanded to Statement, Assign(Variable, Operator(+, ...))
    Assign(Token, Box<Statement>),
    AssignReturn(Token, Box<Statement>), // e.g. ++i or (i = i + 1)

    Operator(Category, Vec<Statement>),

    If(Box<Statement>, Box<Statement>, Option<Box<Statement>>),
    Block(Vec<Statement>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variable<'a> {
    name: &'a str,
    token: &'a Token,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Functions {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenError {
    reason: String,
    token: Option<Token>,
    position: Option<Range<usize>>,
}

impl TokenError {
    pub fn unexpected_token(token: Token) -> TokenError {
        TokenError {
            reason: format!("Unexpected Token {:?}", token.category()),
            token: Some(token),
            position: None,
        }
    }

    pub fn unexpected_end(origin: &str) -> TokenError {
        TokenError {
            reason: format!("Unexpected end while {}", origin),
            token: None,
            position: None,
        }
    }
    pub fn unclosed(token: Token) -> TokenError {
        TokenError {
            reason: format!("Unclosed {:?}", token.category()),
            token: Some(token),
            position: None,
        }
    }

    fn missing_semicolon(token: Token, end: Option<Token>) -> TokenError {
        let position = if let Some(et) = end {
            Range {
                start: token.position.0,
                end: et.position.1,
            }
        } else {
            token.range()
        };
        TokenError {
            reason: format!("Missing semicolon {:?}", token.category()),
            token: Some(token),
            position: Some(position),
        }
    }
    fn position(&self) -> (usize, usize) {
        self.token.map(|t| t.position).unwrap_or((0, 0))
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn range(&self) -> Range<usize> {
        match self.position.clone() {
            Some(x) => x,
            None => {
                let (start, end) = self.position();
                Range { start, end }
            }
        }
    }
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl Error for TokenError {}

pub struct Parser<'a> {
    tokenizer: Tokenizer<'a>,
    root: BlockDepth,
}

impl<'a> Parser<'a> {
    fn parse_keyword(&mut self, token: Token, keyword: Keyword) -> Result<Statement, TokenError> {
        match keyword {
            Keyword::If => self.parse_if(token),
            Keyword::For => Err(TokenError::unexpected_token(token)),
            Keyword::ForEach => Err(TokenError::unexpected_token(token)),
            Keyword::Else => Err(TokenError::unexpected_token(token)),
            Keyword::While => Err(TokenError::unexpected_token(token)),
            Keyword::Repeat => Err(TokenError::unexpected_token(token)),
            Keyword::Until => Err(TokenError::unexpected_token(token)),
            Keyword::LocalVar => Err(TokenError::unexpected_token(token)),
            Keyword::GlobalVar => Err(TokenError::unexpected_token(token)),
            Keyword::Null => Err(TokenError::unexpected_token(token)),
            Keyword::Return => Err(TokenError::unexpected_token(token)),
            Keyword::Include => Err(TokenError::unexpected_token(token)),
            Keyword::Exit => Err(TokenError::unexpected_token(token)),
        }
    }

    fn next_token_as_result(&mut self) -> Result<Token, TokenError> {
        match self.tokenizer.next() {
            Some(token) => Ok(token),
            None => Err(TokenError::unexpected_end("parsing")),
        }
    }

    fn parse_expression_count(
        &mut self,
        increase_when: Category,
        reduce_when: Category,
    ) -> Result<Statement, TokenError> {
        let mut count = 1;
        let next = self.next_token_as_result()?;
        self.parse_expression(next, |t| {
            if t == increase_when {
                count += 1;
            } else if t == reduce_when {
                count -= 1;
            }
            count == 0
        })
    }

    fn parse_if(&mut self, token: Token) -> Result<Statement, TokenError> {
        let left_paren = self.next_token_as_result()?;
        if left_paren.category() != Category::LeftParen {
            return Err(TokenError::unexpected_token(left_paren));
        }
        let condition = self.parse_expression_count(Category::LeftParen, Category::RightParen)?;
        let token = self.next_token_as_result()?;
        println!("I am at: {:?} -> {:?}", token, condition);
        let body = {
            if token.category() == Category::LeftCurlyBracket {
                todo!()
            } else {
                self.parse_token(token)
            }
        }?;
        // TODO else
        Ok(Statement::If(Box::new(condition), Box::new(body), None))
    }

    fn parse_expression(
        &mut self,
        token: Token,
        mut predicate: impl FnMut(Category) -> bool,
    ) -> Result<Statement, TokenError> {
        let mut tokens = vec![token];
        for token in self.tokenizer.by_ref() {
            if !predicate(token.category()) {
                tokens.push(token);
            } else {
                return operator_precedence_parser::expression(tokens);
            }
        }
        Err(TokenError::missing_semicolon(token, tokens.last().cloned()))
    }

    fn parse_token(&mut self, token: Token) -> Result<Statement, TokenError> {
        match token.category() {
            Category::Identifier(Some(keyword)) => self.parse_keyword(token, keyword),
            _ => self.parse_expression(token, |c| c == Category::Semicolon),
        }
    }
}

impl<'a> Iterator for Parser<'a> {
    type Item = Result<Statement, TokenError>;

    fn next(&mut self) -> Option<Self::Item> {
        let token = self.tokenizer.next()?;
        Some(self.parse_token(token))
    }
}

pub fn parse<'a>(code: &'a str) -> Parser<'a> {
    let tokenizer = Tokenizer::new(code);
    let root = (0, 0);
    Parser { tokenizer, root }
}

#[cfg(test)]
mod tests {
    use crate::token::{Base, Category, StringCategory, Token};

    use super::*;
    use Category::*;
    use Statement::*;
    use StringCategory::*;

    #[test]
    fn if_statement() {
        let result = parse(
            "if (description)\nscript_oid(\"1.3.6.1.4.1.25623.1.0.100196\");\n",
        )
        .next()
        .unwrap()
        .unwrap();
        let expected = If(
            Box::new(Variable(Token {
                category: Identifier(None),
                position: (4, 15),
            })),
            Box::new(Call(
                Token {
                    category: Identifier(None),
                    position: (17, 27),
                },
                Box::new(Primitive(Token {
                    category: String(Unquoteable),
                    position: (29, 57),
                })),
            )),
            None,
        );
        assert_eq!(result, expected)
    }
}
