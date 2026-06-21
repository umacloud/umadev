---
id: test-strategy
title: QA Lead — Test Strategy
domain: experts
category: qa-lead
difficulty: intermediate
tags: [acceptance, cases, criteria, experts, from, integration, pyramid, standards]
quality_score: 70
last_updated: 2026-06-15
---
# QA Lead — Test Strategy

## 测试分层底线（按架构分层去测）

详见标准《测试策略与分层规范》(`testing/01-standards/test-strategy-and-layering`)。硬性底线：

- **金字塔**：大量单元（领域逻辑/纯函数，无 IO）+ 适量集成（服务+仓储+真 DB）+ 少量 E2E（关键业务流）。倒金字塔（一堆 e2e、几乎无单测）不合格。
- **各层测各层**：领域层测不变量/状态机；服务层 mock 依赖测用例编排与错误路径；repository 用真 DB 集成测；接口层测状态码/校验/错误信封/鉴权；关键流走 E2E。
- **写法**：AAA、一测一行为、覆盖正常+边界+错误、测行为不测私有实现、测试独立可并行无 flaky、外部依赖用替身。
- **CI**：每次 PR 跑 lint+单元+集成，失败阻断合并；关键路径覆盖达阈值；flaky 必修。

## Test Pyramid

```
        ╱ E2E Tests ╲           (few, slow, expensive)
       ╱─────────────╲
      ╱ Integration   ╲        (moderate count)
     ╱─────────────────╲
    ╱   Unit Tests      ╲      (many, fast, cheap)
   ╱─────────────────────╲
```

Target ratios: 70% unit / 20% integration / 10% E2E

## From Acceptance Criteria to Test Cases

Each PRD acceptance criteria generates multiple test cases:

**AC**: Given a user on the login page, when they enter valid credentials, then they are redirected to /dashboard

**Test Cases**:
1. ✓ Valid email + correct password → redirect to /dashboard
2. ✓ Valid email + wrong password → show error "Invalid credentials"
3. ✓ Non-existent email → show same generic error (no user enumeration)
4. ✓ Empty email field → show validation "Email is required"
5. ✓ Invalid email format → show validation "Enter a valid email"
6. ✓ Empty password → show validation "Password is required"
7. ✓ 5 failed attempts → show "Account locked, try again in 15 minutes"
8. ✓ SQL injection in email field → sanitized, returns validation error
9. ✓ XSS in email field → sanitized, no script execution
10. ✓ Redirect to originally requested page after login (not always /dashboard)

## Unit Test Standards

### Naming Convention
```
test_[unit]_[scenario]_[expected_result]

test_login_valid_credentials_returns_jwt
test_login_wrong_password_returns_401
test_login_locked_account_returns_429
```

### Test Structure (Arrange-Act-Assert)
```
// Arrange: set up test data and dependencies
let user = create_test_user("test@example.com", "password123");
let req = LoginRequest { email: "test@example.com", password: "password123" };

// Act: call the function under test
let result = auth_service.login(req).await;

// Assert: verify the outcome
assert!(result.is_ok());
assert!(!result.unwrap().token.is_empty());
```

### What to Test
- Happy path (normal operation)
- Boundary values (0, 1, max, max+1)
- Error paths (invalid input, missing data, network failure)
- Edge cases (empty collections, null/None, concurrent access)

### What NOT to Test
- Third-party library internals
- Private methods directly (test through public API)
- Configuration / constants
- Framework boilerplate

## Integration Test Standards

### API endpoint tests must verify:
1. Correct status code
2. Response body structure (schema validation)
3. Database state after mutation
4. Authentication/authorization enforcement
5. Error responses for invalid input

### Database test isolation:
- Each test uses a transaction that rolls back after
- OR each test uses a fresh test database
- Never share state between tests

## E2E Test Standards

### What to cover:
- Complete user flows (signup → onboard → core action → logout)
- Cross-page navigation
- Form submissions with validation
- Real API calls (not mocked)

### What NOT to E2E test:
- Every field validation (unit test those)
- Error edge cases (integration test those)
- Visual appearance (use visual regression tools separately)

## Pre-Release Checklist

- [ ] All unit tests pass
- [ ] All integration tests pass
- [ ] E2E smoke tests pass
- [ ] No console errors in browser
- [ ] Performance budget met (Lighthouse ≥ 90)
- [ ] Accessibility audit passes (axe-core, 0 violations)
- [ ] Security headers present
- [ ] Error tracking connected and receiving events
- [ ] Monitoring dashboards show expected metrics
- [ ] Rollback procedure documented and tested
