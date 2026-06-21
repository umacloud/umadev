---
id: backend-framework-idioms
title: 后端框架地道写法（Spring/NestJS/FastAPI/Express/Go · 官方惯例）
domain: backend
category: 01-standards
difficulty: intermediate
tags: [后端框架, spring-boot, nestjs, fastapi, express, gin, go, 分层, 依赖注入, dto, 事务, 中间件, 官方惯例, 商业级]
quality_score: 94
last_updated: 2026-06-19
---

# 后端框架地道写法（Spring/NestJS/FastAPI/Express/Go · 官方惯例）

> 通用分层(controller→service→domain→repository)要落到**各框架的地道写法**。纯底座常写出"能跑但不地道"的代码。本标准把分层映射到主流框架的官方惯例。先按所选框架对号入座。

## 通用（所有框架都遵守）

- 四层 + 依赖向内（见《应用分层与分包》）：controller 仅传输 / service 用例+事务+收发 DTO / domain 业务规则 / repository 持久化。
- **按 feature 分包**（不按技术层堆大目录）；shared 放公共件。
- 入参 DTO + 校验；统一错误处理；DI 注入接口而非具体类。

## Spring Boot（Java）

- 分层注解：`@RestController`(传输) → `@Service`(业务+`@Transactional` 事务边界) → `@Repository`(Spring Data JPA)。
- DTO + `@Valid` + Bean Validation 注解校验；**实体(@Entity) 不直接当响应**，转 DTO。
- 全局异常用 `@RestControllerAdvice` + `@ExceptionHandler` 集中映射 HTTP。
- 构造器注入(不用字段 @Autowired)；`application.yml` + profiles 分环境；配置类型安全(@ConfigurationProperties)。
- 分包按 feature（`com.app.orders.{web,service,domain,repo}`）优于按层大目录。

## NestJS（TypeScript）

- **模块化按 feature**：每个 feature 一个 `@Module`，含 controller/service/dto/entity（**超过 ~20 controller 的纯分层会后悔**——按 feature）。
- `@Controller`(传输) → `@Injectable() Service`(用例) → Repository(TypeORM/Prisma)；DI 注入。
- DTO + **class-validator + ValidationPipe**(全局)校验；不返回 ORM entity，转 DTO/序列化。
- 横切用 **Guards**(鉴权/授权)、**Interceptors**(日志/转换)、**Pipes**(校验/转换)、**ExceptionFilter**(统一错误)；放 SharedModule。
- 事务在 service 层（QueryRunner / Prisma `$transaction`）。

## FastAPI（Python）

- 分层：`routers`(端点) → `services`(业务工作流) → `repositories`(DB 查询) → `schemas`(Pydantic 请求/响应校验)；core/db/shared 放基础设施；**business 按 feature 文件夹**。
- **Pydantic 模型**做入参/出参校验与序列化；响应模型用 `response_model`，**不直接吐 ORM 对象**。
- **依赖注入用 `Depends`**（DB session、当前用户、权限）；I/O 用 `async`。
- SQLAlchemy 在 repository 封装；事务边界在 service；统一异常处理(exception_handler)。
- API 版本化(`/v1`)、自动 OpenAPI 文档。

## Express / NestJS-less Node（TypeScript）

- 不要 fat route：route → controller → service → repository 分层；逻辑不写在路由回调里。
- 校验用 zod/Joi 中间件；统一 **error-handling middleware**(最后挂载)；async 错误用包装器/`express-async-errors` 传到错误中间件。
- 中间件做鉴权/日志/限流；配置/DI 显式装配；按 feature 组织。

## Go（Gin/Echo/标准库）

- Clean Architecture：handler(传输) → usecase/service(业务) → repository(接口+实现) → domain(实体)；依赖倒置，依赖注入接口。
- 用 `context.Context` 传递取消/超时/请求范围值；**显式错误处理**(`if err != nil`)，错误包装(`fmt.Errorf("%w")`)。
- 入参绑定+校验(validator)；不返回 DB model 直接序列化；事务在 service。
- 不用全局可变状态；并发用 goroutine+channel 且注意同步。

## 反模式（出现即不合格）

- 把业务逻辑写在 controller/router/handler；controller 直连 DB/ORM。
- 返回 ORM entity/DB model 给客户端；不转 DTO。
- 不用框架的校验/DI/异常机制(各写各的)；事务写在错误层级。
- 按技术层堆大目录(controllers/services/repos)而非 feature（NestJS/FastAPI 尤其）。
- Spring 字段注入；Go 忽略 err / 全局可变状态；Express fat route 无错误中间件。

## 最低交付 checklist

- [ ] 按所选框架地道分层：Spring(@RestController/@Service/@Transactional/@Repository+@RestControllerAdvice)、NestJS(Module/Controller/Service/DTO+ValidationPipe/Guards/Filters)、FastAPI(routers/services/repositories/Pydantic/Depends/async)、Express(分层+错误中间件)、Go(clean arch+context+显式 err)。
- [ ] 按 feature 分包；DTO 校验+不泄露 entity；DI 注入接口；事务在 service 层。
- [ ] 框架原生机制做横切(校验/鉴权/异常/中间件)，不各写各的。

---
**参考（官方）**：Spring Boot 官方/分层、NestJS 官方(Modules/Providers/Pipes/Guards)、FastAPI 官方(Bigger Applications/Dependencies/SQL)、Express 错误处理、Go Clean Architecture。
