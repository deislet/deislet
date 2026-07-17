use std::fmt::{self, Display};
use std::str::FromStr;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MIMETypes {
    TextPlain,
    TextHTML,
    TextCSS,
    TextJavaScript,
    TextXML,
    ApplicationJavaScript,
    ApplicationJSON,
    ApplicationXML,
    ApplicationXJavaScript,
    ApplicationXJSON,
}

impl FromStr for MIMETypes {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text/plain" => Ok(MIMETypes::TextPlain),
            "text/html" => Ok(MIMETypes::TextHTML),
            "text/css" => Ok(MIMETypes::TextCSS),
            "text/javascript" => Ok(MIMETypes::TextJavaScript),
            "text/xml" => Ok(MIMETypes::TextXML),
            "application/javascript" => Ok(MIMETypes::ApplicationJavaScript),
            "application/json" => Ok(MIMETypes::ApplicationJSON),
            "application/xml" => Ok(MIMETypes::ApplicationXML),
            "application/x-javascript" => Ok(MIMETypes::ApplicationXJavaScript),
            "application/x-json" => Ok(MIMETypes::ApplicationXJSON),
            _ => Err(()),
        }
    }
}

impl Display for MIMETypes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MIMETypes::TextPlain => write!(f, "text/plain"),
            MIMETypes::TextHTML => write!(f, "text/html"),
            MIMETypes::TextCSS => write!(f, "text/css"),
            MIMETypes::TextJavaScript => write!(f, "text/javascript"),
            MIMETypes::TextXML => write!(f, "text/xml"),
            MIMETypes::ApplicationJavaScript => write!(f, "application/javascript"),
            MIMETypes::ApplicationJSON => write!(f, "application/json"),
            MIMETypes::ApplicationXML => write!(f, "application/xml"),
            MIMETypes::ApplicationXJavaScript => write!(f, "application/x-javascript"),
            MIMETypes::ApplicationXJSON => write!(f, "application/x-json"),
        }
    }
}
