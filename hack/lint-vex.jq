# Validate the OpenVEX document: the required fields of the OpenVEX v0.2.0
# specification, and versioned cargo package URLs of the affected
# dependencies as products, so that statements age out with version bumps.
def check(cond; msg): if cond then . else error(msg) end;

check(."@context" == "https://openvex.dev/ns/v0.2.0"; "@context must be https://openvex.dev/ns/v0.2.0")
| check(."@id" | type == "string" and length > 0; "@id is required")
| check(.author | type == "string" and length > 0; "author is required")
| check(.timestamp | type == "string" and test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T"); "timestamp must be RFC 3339")
| check(.version | type == "number" and . >= 1; "version must be a positive integer")
| check(.statements | type == "array"; "statements must be an array")
| all(.statements[];
    check(.vulnerability.name | type == "string" and length > 0; "vulnerability.name is required")
    | check([.products[]?."@id"] | length > 0 and all(test("^pkg:cargo/[^@]+@[^@]+$")); "products must be versioned pkg:cargo package URLs")
    | check(.status | IN("not_affected", "affected", "fixed", "under_investigation"); "status is invalid")
    | check(.status != "not_affected" or .justification != null or .impact_statement != null; "not_affected needs a justification or impact_statement")
    | check(.status != "affected" or .action_statement != null; "affected needs an action_statement")
    | true)
