Antigravity Tools - Linux Headless (WebUI)

启动:
  ./start.sh

或:
  ABV_DIST_PATH=./dist PORT=8045 ./antigravity-tools --headless

访问:
  http://localhost:8045

可选环境变量:
  API_KEY / ABV_API_KEY          API 鉴权密钥
  WEB_PASSWORD / ABV_WEB_PASSWORD  Web 登录密码
  PORT                           默认 8045
  ABV_DIST_PATH                  前端静态资源目录

数据目录:
  ~/.antigravity_tools
