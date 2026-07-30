local M = {}
local preview_components = require("preview-components")
M.declaration = {
  preview = preview_components.input_output(), name="collected-prompt", type_variables={"T"},
  semantic={input={kind="list",item={kind="variable",name="T"}},
    output={kind="named",name="nefor.contracts.ProviderInput",arguments={}},
    inputs={{wire="nefor.dynamic.Collected",type={kind="list",item={kind="variable",name="T"}}}},
    outputs={{wire="generic-provider.ProviderOut",type={kind="named",name="nefor.contracts.ProviderInput",arguments={}}}}}, params={},
  inputs={values="nefor.dynamic.Collected"}, outputs={"generic-provider.ProviderOut"}, signals={} }
function M.construct(id, params, emit)
  local instance={id=id}
  function instance.deliver(a)
    local content=a.messages[1].message.value
    emit({kind="generic-provider.ProviderOut",from=id,value={content=content},
      messages={{role="user",content=content}}})
    return {status="ok"}
  end
  emit({kind="mag.ready",from=id}); return instance
end
return M
