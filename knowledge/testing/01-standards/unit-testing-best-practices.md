---
id: unit-testing-best-practices
title: 单元测试最佳实践完全指南
domain: testing
category: 01-standards
difficulty: intermediate
tags: [testing, unit-test, tdd, best-practices]
quality_score: 92
maintainer: testing-team@umadev.com
last_updated: 2026-03-29
---

# 单元测试最佳实践完全指南

## 核心原则

### 1. 测试金字塔
- **单元测试**: 70% - 快速,隔离,低成本
- **集成测试**: 20% - 中速,多组件
- **E2E测试**: 10% - 慢速,完整流程

### 2. FIRST 原则
- **Fast**: 毫秒级执行
- **Isolated**: 隔离依赖
- **Repeatable**: 结果稳定
- **Self-validating**: 自动验证

### 3. AAA 模式
```python
def test_user_creation():
    # Arrange
    user_data = {"name": "Alice", "email": "alice@example.com"}
    mock_db = Mock()
    
    # Act
    result = create_user(user_data, mock_db)
    
    # Assert
    assert result.id is not None
    assert result.email == "alice@example.com"
    mock_db.insert.assert_called_once()
```

## 实战示例 (Python/pytest)

```python
import pytest
from unittest.mock import Mock, patch

class TestUserService:
    @pytest.fixture
    def mock_db(self):
        return Mock()
    
    @pytest.fixture
    def user_service(self, mock_db):
        return UserService(mock_db)
    
    def test_create_user_success(self, user_service):
        # Arrange
        user_data = {
            "email": "test@example.com",
            "name": "Test User"
        }
        
        # Act
        result = user_service.create(user_data)
        
        # Assert
        assert result.id is not None
        assert result.email == "test@example.com"
    
    def test_create_user_invalid_email(self, user_service):
        # Arrange
        user_data = {"email": "invalid", "name": "Test"}
        
        # Act & Assert
        with pytest.raises(ValidationError):
            user_service.create(user_data)
    
    @patch('services.email.send')
    def test_send_welcome_email(self, mock_send, user_service):
        # Arrange
        user = User(id=1, email="test@example.com")
        
        # Act
        user_service.send_welcome(user)
        
        # Assert
        mock_send.assert_called_once_with(
            to="test@example.com",
            subject="Welcome!"
        )

# 参数化测试
@pytest.mark.parametrize("email,expected", [
    ("test@example.com", True),
    ("invalid", False),
    ("", False),
])
def test_validate_email(email, expected):
    assert validate_email(email) == expected
```

## 最佳实践

### ✅ DO
- 测试边界条件
- Mock 外部依赖
- 描述性测试名
- 一个测试一个断言类型
- 快速反馈循环

### ❌ DON'T
- 测试实现细节
- 过度 Mock
- 忽略异步错误
- 硬编码测试数据
