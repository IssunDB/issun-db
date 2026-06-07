Feature: MATCH clause conformance

    Scenario: Match a single node by label
        Given an empty graph
        And having executed:
      """
      CREATE (a:Person {name: 'Alice', age: 30})
      """
        When executing query:
      """
      MATCH (n:Person) RETURN n.name AS name, n.age AS age
      """
        Then the result should be:
            | name    | age |
            | 'Alice' | 30  |

    Scenario: Match multiple nodes and filter with WHERE comparisons
        Given an empty graph
        And having executed:
      """
      CREATE (a:Person {name: 'Alice', age: 30})
      """
        And having executed:
      """
      CREATE (b:Person {name: 'Bob', age: 25})
      """
        When executing query:
      """
      MATCH (n:Person) WHERE n.age > 26 RETURN n.name AS name
      """
        Then the result should be:
            | name    |
            | 'Alice' |

    Scenario: Expand relationships across nodes
        Given an empty graph
        And having executed:
      """
      CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})
      """
        When executing query:
      """
      MATCH (p1:Person)-[:KNOWS]->(p2:Person) RETURN p1.name AS src, p2.name AS dst
      """
        Then the result should be:
            | src     | dst   |
            | 'Alice' | 'Bob' |
