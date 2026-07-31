// The Cypher formatter.
//
// Its own module so `scripts/check_playground.mjs` can import it. That check is the whole safety
// argument for running a formatter over a query: it runs every string in `demos.js` before and
// after formatting and compares the rows, so a casing or line-breaking rule that changed what a
// query means fails the build. Reaching the formatter from `app.js` was impossible while it lived
// there, since that module touches the DOM at import time and a Node process has none.

// Clause phrases that begin a line, longest first so `ON CREATE SET` is recognized before the `SET`
// inside it.
const CLAUSE_PHRASES = [
    ["ON", "CREATE", "SET"],
    ["ON", "MATCH", "SET"],
    ["OPTIONAL", "MATCH"],
    ["DETACH", "DELETE"],
    ["ORDER", "BY"],
    ["UNION", "ALL"],
    ["MATCH"],
    ["WHERE"],
    ["WITH"],
    ["RETURN"],
    ["SKIP"],
    ["LIMIT"],
    ["CREATE"],
    ["MERGE"],
    ["SET"],
    ["REMOVE"],
    ["DELETE"],
    ["UNWIND"],
    ["CALL"],
    ["YIELD"],
    ["UNION"],
    ["FOREACH"],
];

// The clauses whose comma-separated items are patterns rather than expressions. Breaking after each
// comma there turns a long line into a readable list of paths; doing it in RETURN would scatter a
// projection over as many lines as it has columns.
const PATTERN_CLAUSES = new Set(["CREATE", "MERGE"]);

// Deliberately much narrower than the highlighter's keyword set. Uppercasing everything that set
// contains rewrote `issundb.shortestPath` to `issundb.SHORTESTPATH`, and the yield fields `index`
// and `count` to `INDEX` and `COUNT`, all three of which are case-sensitive names rather than
// syntax. So only operators are listed here, and a clause word is uppercased because the phrase
// scan recognized it as one, not because it appears in a list. Function names are left alone: an
// aggregate is conventionally lowercase, and `all(` is not the `ALL` of `UNION ALL`.
const FORMAT_UPPERCASE = new Set([
    "and",
    "or",
    "xor",
    "not",
    "in",
    "is",
    "null",
    "true",
    "false",
    "distinct",
    "as",
    "asc",
    "desc",
    "ascending",
    "descending",
    "starts",
    "ends",
    "contains",
]);

const FORMAT_TOKEN = new RegExp(
    [
        "(\\/\\/[^\\n]*|\\/\\*[\\s\\S]*?\\*\\/)",
        "('(?:[^'\\\\]|\\\\.)*'|\"(?:[^\"\\\\]|\\\\.)*\")",
        "([A-Za-z_]\\w*)",
        "(\\s+)",
        "([^\\s])",
    ].join("|"),
    "g",
);

// Line breaking and keyword casing, and nothing else. Spacing within a line is left as written apart
// from collapsing runs of whitespace, because re-spacing would have to know that the `-` in
// `-[:KNOWS]->` and the `*` in `[r*1..3]` are not binary operators. That restraint is what makes the
// pass safe to run on any query: it cannot change what the query means.
export function formatCypher(src) {
    const tokens = [...src.matchAll(FORMAT_TOKEN)].map((m) => ({
        comment: m[1],
        string: m[2],
        word: m[3],
        space: m[4],
        other: m[5],
        text: m[0],
    }));

    // A bracket depth per token, so a clause word inside a pattern or a map is not mistaken for the
    // start of a line, and the index of every word, so a phrase can be matched by lookahead.
    let depth = 0;
    const words = [];
    tokens.forEach((token, i) => {
        token.depth = depth;
        if (token.other && "([{".includes(token.other)) depth += 1;
        if (token.other && ")]}".includes(token.other)) depth -= 1;
        if (token.word) words.push(i);
    });

    const previousWordOf = (index) => {
        for (let j = index - 1; j >= 0; j -= 1) {
            if (tokens[j].space || tokens[j].comment) continue;
            return tokens[j];
        }
        return null;
    };

    const nextNonSpaceOf = (index) => {
        for (let j = index + 1; j < tokens.length; j += 1) {
            if (tokens[j].space) continue;
            return tokens[j];
        }
        return null;
    };

    // `n.set` and `:Match` are names. Guarding the phrase scan and not only the casing is what stops
    // `RETURN n.set` from being broken across two lines at the property.
    const isQualifiedName = (index) => {
        const previous = previousWordOf(index);
        if (previous && (previous.other === "." || previous.other === ":")) return true;
        return Boolean(previous && previous.word && previous.word.toLowerCase() === "as");
    };

    const upperOf = (index) => (index === undefined ? "" : tokens[index].word.toUpperCase());
    const breakAt = new Set();
    const consumed = new Set();
    const phraseWords = new Set();
    words.forEach((i, w) => {
        if (consumed.has(i) || tokens[i].depth !== 0 || isQualifiedName(i)) return;
        const phrase = CLAUSE_PHRASES.find((candidate) =>
            candidate.every((word, k) => upperOf(words[w + k]) === word),
        );
        if (!phrase) return;
        breakAt.add(i);
        tokens[i].clause = phrase.join(" ");
        phrase.forEach((_, k) => phraseWords.add(words[w + k]));
        for (let k = 1; k < phrase.length; k += 1) consumed.add(words[w + k]);
    });

    function shouldUppercase(index) {
        if (phraseWords.has(index)) return true;
        if (!FORMAT_UPPERCASE.has(tokens[index].word.toLowerCase())) return false;
        if (isQualifiedName(index)) return false;
        // A word the phrase scan did not claim, followed by an open parenthesis, is a function name
        // rather than an operator. A clause keyword is exempt, since `MATCH (` is still a clause.
        const next = nextNonSpaceOf(index);
        return !(next && next.other === "(");
    }

    let out = "";
    let atLineStart = true;
    let pendingSpace = false;
    let clause = "";

    const newline = () => {
        if (!atLineStart) out += "\n";
        atLineStart = true;
        pendingSpace = false;
    };

    tokens.forEach((token, i) => {
        if (token.space) {
            pendingSpace = out.length > 0;
            return;
        }

        // A comment runs to the end of its line, so it has to keep one to itself or it would swallow
        // whatever the formatter put after it.
        if (token.comment) {
            newline();
            out += token.text;
            out += "\n";
            atLineStart = true;
            return;
        }

        if (breakAt.has(i)) {
            newline();
            clause = token.clause;
        }

        if (pendingSpace && !atLineStart) out += " ";
        pendingSpace = false;

        if (token.word) {
            out += shouldUppercase(i) ? token.word.toUpperCase() : token.word;
            atLineStart = false;
            return;
        }

        if (token.other === ";" && token.depth === 0) {
            out += ";\n";
            atLineStart = true;
            clause = "";
            return;
        }

        if (token.other === "," && token.depth === 0 && PATTERN_CLAUSES.has(clause)) {
            out += ",\n" + " ".repeat(clause.length + 1);
            atLineStart = true;
            return;
        }

        out += token.text;
        atLineStart = false;
    });

    return out.replace(/[ \t]+$/gm, "").trim();
}
