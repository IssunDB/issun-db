Feature: WITH and UNWIND clause conformance

  Scenario: Strict projection barrier with WITH
    Given an empty graph
    And having executed:
      """
      CREATE (a:Person {name: 'Alice', age: 30})
      """
    When executing query:
      """
      MATCH (p:Person)
      WITH p, p.age AS age
      WHERE age > 26
      RETURN p.name AS name, age
      """
    Then the result should be:
      | name    | age |
      | 'Alice' | 30  |

  Scenario: Unwind an array literal
    Given an empty graph
    When executing query:
      """
      UNWIND [10, 20, 30] AS val
      RETURN val
      """
    Then the result should be:
      | val |
      | 10  |
      | 20  |
      | 30  |

  Scenario: Unwind and join with MATCH
    Given an empty graph
    And having executed:
      """
      CREATE (a:Person {name: 'Alice'})
      """
    And having executed:
      """
      CREATE (b:Person {name: 'Bob'})
      """
    When executing query:
      """
      UNWIND ['Alice', 'Bob'] AS name
      MATCH (p:Person)
      WHERE p.name = name
      RETURN p.name AS name
      """
    Then the result should be:
      | name    |
      | 'Alice' |
      | 'Bob'   |
