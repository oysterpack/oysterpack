Feature: [01D4RWGKRYAJCQ4Q5SD3Z6WG6P] When all ReqRep client references fall out of scope, then the backend service will automatically shutdown

  The backend service will continue to run as long as ReqRep client references are alive.

  Scenario: [01D4RWJMA0THQQPSF2XQ6Q8AM1] Drop all ReqRep client instances
    Given [01D4RWJMA0THQQPSF2XQ6Q8AM1] a ReqRep client
    When [01D4RWJMA0THQQPSF2XQ6Q8AM1] the client is dropped
    Then [01D4RWJMA0THQQPSF2XQ6Q8AM1] the backend service shutsdown