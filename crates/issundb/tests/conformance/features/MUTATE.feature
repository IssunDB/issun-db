Feature: Graph mutation conformance

  Scenario: Create nodes and relationships
    Given an empty graph
    And having executed:
      """
      CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})
      """
    When executing query:
      """
      MATCH (p:Person) RETURN p.name AS name
      """
    Then the result should be:
      | name    |
      | 'Alice' |
      | 'Bob'   |

  Scenario: Update node properties with SET
    Given an empty graph
    And having executed:
      """
      CREATE (a:Person {name: 'Alice', age: 30})
      """
    And having executed:
      """
      MATCH (p:Person) WHERE p.name = 'Alice' SET p.age = 31
      """
    When executing query:
      """
      MATCH (p:Person) RETURN p.name AS name, p.age AS age
      """
    Then the result should be:
      | name    | age |
      | 'Alice' | 31  |

  Scenario: Delete matched nodes
    Given an empty graph
    And having executed:
      """
      CREATE (a:Person {name: 'Alice'})
      """
    And having executed:
      """
      CREATE (b:Person {name: 'Bob'})
      """
    And having executed:
      """
      MATCH (p:Person) WHERE p.name = 'Alice' DELETE p
      """
    When executing query:
      """
      MATCH (p:Person) RETURN p.name AS name
      """
    Then the result should be:
      | name  |
      | 'Bob' |
