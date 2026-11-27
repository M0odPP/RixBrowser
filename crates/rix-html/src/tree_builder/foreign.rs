//! WHATWG HTML5 §13.2.6.5 SVG, MathML, and foreign attribute/tag case adjustments.

use html5ever::tokenizer::Tag;
use markup5ever::{local_name, namespace_prefix, ns, Attribute, LocalName, QualName};

macro_rules! qualname {
    ("", $local:tt) => {
        QualName {
            prefix: None,
            ns: ns!(),
            local: local_name!($local),
        }
    };
    ($prefix:tt $ns:tt $local:tt) => {
        QualName {
            prefix: Some(namespace_prefix!($prefix)),
            ns: ns!($ns),
            local: local_name!($local),
        }
    };
}

/// WHATWG HTML §13.2.6.5 SVG tag-name case adjustment table.
pub fn adjust_svg_tag_name(tag: &mut Tag) {
    match tag.name {
        local_name!("altglyph") => tag.name = local_name!("altGlyph"),
        local_name!("altglyphdef") => tag.name = local_name!("altGlyphDef"),
        local_name!("altglyphitem") => tag.name = local_name!("altGlyphItem"),
        local_name!("animatecolor") => tag.name = local_name!("animateColor"),
        local_name!("animatemotion") => tag.name = local_name!("animateMotion"),
        local_name!("animatetransform") => tag.name = local_name!("animateTransform"),
        local_name!("clippath") => tag.name = local_name!("clipPath"),
        local_name!("feblend") => tag.name = local_name!("feBlend"),
        local_name!("fecolormatrix") => tag.name = local_name!("feColorMatrix"),
        local_name!("fecomponenttransfer") => tag.name = local_name!("feComponentTransfer"),
        local_name!("fecomposite") => tag.name = local_name!("feComposite"),
        local_name!("feconvolvematrix") => tag.name = local_name!("feConvolveMatrix"),
        local_name!("fediffuselighting") => tag.name = local_name!("feDiffuseLighting"),
        local_name!("fedisplacementmap") => tag.name = local_name!("feDisplacementMap"),
        local_name!("fedistantlight") => tag.name = local_name!("feDistantLight"),
        local_name!("fedropshadow") => tag.name = local_name!("feDropShadow"),
        local_name!("feflood") => tag.name = local_name!("feFlood"),
        local_name!("fefunca") => tag.name = local_name!("feFuncA"),
        local_name!("fefuncb") => tag.name = local_name!("feFuncB"),
        local_name!("fefuncg") => tag.name = local_name!("feFuncG"),
        local_name!("fefuncr") => tag.name = local_name!("feFuncR"),
        local_name!("fegaussianblur") => tag.name = local_name!("feGaussianBlur"),
        local_name!("feimage") => tag.name = local_name!("feImage"),
        local_name!("femerge") => tag.name = local_name!("feMerge"),
        local_name!("femergenode") => tag.name = local_name!("feMergeNode"),
        local_name!("femorphology") => tag.name = local_name!("feMorphology"),
        local_name!("feoffset") => tag.name = local_name!("feOffset"),
        local_name!("fepointlight") => tag.name = local_name!("fePointLight"),
        local_name!("fespecularlighting") => tag.name = local_name!("feSpecularLighting"),
        local_name!("fespotlight") => tag.name = local_name!("feSpotLight"),
        local_name!("fetile") => tag.name = local_name!("feTile"),
        local_name!("feturbulence") => tag.name = local_name!("feTurbulence"),
        local_name!("foreignobject") => tag.name = local_name!("foreignObject"),
        local_name!("glyphref") => tag.name = local_name!("glyphRef"),
        local_name!("lineargradient") => tag.name = local_name!("linearGradient"),
        local_name!("radialgradient") => tag.name = local_name!("radialGradient"),
        local_name!("textpath") => tag.name = local_name!("textPath"),
        _ => {}
    }
}

fn adjust_attributes<F>(tag: &mut Tag, mut map: F)
where
    F: FnMut(LocalName) -> Option<QualName>,
{
    for Attribute { name, .. } in &mut tag.attrs {
        if let Some(replacement) = map(name.local.clone()) {
            *name = replacement;
        }
    }
}

/// WHATWG HTML §13.2.6.5 SVG attribute case adjustment table.
pub fn adjust_svg_attributes(tag: &mut Tag) {
    adjust_attributes(tag, |k| match k {
        local_name!("attributename") => Some(qualname!("", "attributeName")),
        local_name!("attributetype") => Some(qualname!("", "attributeType")),
        local_name!("basefrequency") => Some(qualname!("", "baseFrequency")),
        local_name!("baseprofile") => Some(qualname!("", "baseProfile")),
        local_name!("calcmode") => Some(qualname!("", "calcMode")),
        local_name!("clippathunits") => Some(qualname!("", "clipPathUnits")),
        local_name!("diffuseconstant") => Some(qualname!("", "diffuseConstant")),
        local_name!("edgemode") => Some(qualname!("", "edgeMode")),
        local_name!("filterunits") => Some(qualname!("", "filterUnits")),
        local_name!("glyphref") => Some(qualname!("", "glyphRef")),
        local_name!("gradienttransform") => Some(qualname!("", "gradientTransform")),
        local_name!("gradientunits") => Some(qualname!("", "gradientUnits")),
        local_name!("kernelmatrix") => Some(qualname!("", "kernelMatrix")),
        local_name!("kernelunitlength") => Some(qualname!("", "kernelUnitLength")),
        local_name!("keypoints") => Some(qualname!("", "keyPoints")),
        local_name!("keysplines") => Some(qualname!("", "keySplines")),
        local_name!("keytimes") => Some(qualname!("", "keyTimes")),
        local_name!("lengthadjust") => Some(qualname!("", "lengthAdjust")),
        local_name!("limitingconeangle") => Some(qualname!("", "limitingConeAngle")),
        local_name!("markerheight") => Some(qualname!("", "markerHeight")),
        local_name!("markerunits") => Some(qualname!("", "markerUnits")),
        local_name!("markerwidth") => Some(qualname!("", "markerWidth")),
        local_name!("maskcontentunits") => Some(qualname!("", "maskContentUnits")),
        local_name!("maskunits") => Some(qualname!("", "maskUnits")),
        local_name!("numoctaves") => Some(qualname!("", "numOctaves")),
        local_name!("pathlength") => Some(qualname!("", "pathLength")),
        local_name!("patterncontentunits") => Some(qualname!("", "patternContentUnits")),
        local_name!("patterntransform") => Some(qualname!("", "patternTransform")),
        local_name!("patternunits") => Some(qualname!("", "patternUnits")),
        local_name!("pointsatx") => Some(qualname!("", "pointsAtX")),
        local_name!("pointsaty") => Some(qualname!("", "pointsAtY")),
        local_name!("pointsatz") => Some(qualname!("", "pointsAtZ")),
        local_name!("preservealpha") => Some(qualname!("", "preserveAlpha")),
        local_name!("preserveaspectratio") => Some(qualname!("", "preserveAspectRatio")),
        local_name!("primitiveunits") => Some(qualname!("", "primitiveUnits")),
        local_name!("refx") => Some(qualname!("", "refX")),
        local_name!("refy") => Some(qualname!("", "refY")),
        local_name!("repeatcount") => Some(qualname!("", "repeatCount")),
        local_name!("repeatdur") => Some(qualname!("", "repeatDur")),
        local_name!("requiredextensions") => Some(qualname!("", "requiredExtensions")),
        local_name!("requiredfeatures") => Some(qualname!("", "requiredFeatures")),
        local_name!("specularconstant") => Some(qualname!("", "specularConstant")),
        local_name!("specularexponent") => Some(qualname!("", "specularExponent")),
        local_name!("spreadmethod") => Some(qualname!("", "spreadMethod")),
        local_name!("startoffset") => Some(qualname!("", "startOffset")),
        local_name!("stddeviation") => Some(qualname!("", "stdDeviation")),
        local_name!("stitchtiles") => Some(qualname!("", "stitchTiles")),
        local_name!("surfacescale") => Some(qualname!("", "surfaceScale")),
        local_name!("systemlanguage") => Some(qualname!("", "systemLanguage")),
        local_name!("tablevalues") => Some(qualname!("", "tableValues")),
        local_name!("targetx") => Some(qualname!("", "targetX")),
        local_name!("targety") => Some(qualname!("", "targetY")),
        local_name!("textlength") => Some(qualname!("", "textLength")),
        local_name!("viewbox") => Some(qualname!("", "viewBox")),
        local_name!("viewtarget") => Some(qualname!("", "viewTarget")),
        local_name!("xchannelselector") => Some(qualname!("", "xChannelSelector")),
        local_name!("ychannelselector") => Some(qualname!("", "yChannelSelector")),
        local_name!("zoomandpan") => Some(qualname!("", "zoomAndPan")),
        _ => None,
    });
}

/// WHATWG HTML §13.2.6.5 MathML attribute case adjustment table.
pub fn adjust_mathml_attributes(tag: &mut Tag) {
    adjust_attributes(tag, |k| match k {
        local_name!("definitionurl") => Some(qualname!("", "definitionURL")),
        _ => None,
    });
}

/// WHATWG HTML §13.2.6.5 foreign namespace attribute adjustment table.
pub fn adjust_foreign_attributes(tag: &mut Tag) {
    adjust_attributes(tag, |k| match k {
        local_name!("xlink:actuate") => Some(qualname!("xlink" xlink "actuate")),
        local_name!("xlink:arcrole") => Some(qualname!("xlink" xlink "arcrole")),
        local_name!("xlink:href") => Some(qualname!("xlink" xlink "href")),
        local_name!("xlink:role") => Some(qualname!("xlink" xlink "role")),
        local_name!("xlink:show") => Some(qualname!("xlink" xlink "show")),
        local_name!("xlink:title") => Some(qualname!("xlink" xlink "title")),
        local_name!("xlink:type") => Some(qualname!("xlink" xlink "type")),
        local_name!("xml:lang") => Some(qualname!("xml" xml "lang")),
        local_name!("xml:space") => Some(qualname!("xml" xml "space")),
        local_name!("xmlns") => Some(qualname!("" xmlns "xmlns")),
        local_name!("xmlns:xlink") => Some(qualname!("xmlns" xmlns "xlink")),
        _ => None,
    });
}
