{{/*
Expand the name of the chart.
*/}}
{{- define "s4.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Create a fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited.
*/}}
{{- define "s4.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
Chart name + version, for the chart label.
*/}}
{{- define "s4.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Image reference. Falls back to .Chart.AppVersion when image.tag is unset.
*/}}
{{- define "s4.image" -}}
{{- $tag := default .Chart.AppVersion .Values.image.tag -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}

{{/*
Common labels. Recommended set per
https://helm.sh/docs/chart_best_practices/labels/
*/}}
{{- define "s4.labels" -}}
helm.sh/chart: {{ include "s4.chart" . }}
{{ include "s4.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: s4
{{- end -}}

{{/*
Selector labels — must be stable across upgrades.
*/}}
{{- define "s4.selectorLabels" -}}
app.kubernetes.io/name: {{ include "s4.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
ServiceAccount name.
*/}}
{{- define "s4.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "s4.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/*
TLS secret name. Either user-provided existingSecret, or one we render.
*/}}
{{- define "s4.tlsSecretName" -}}
{{- if .Values.tls.existingSecret -}}
{{- .Values.tls.existingSecret -}}
{{- else -}}
{{- printf "%s-tls" (include "s4.fullname" .) -}}
{{- end -}}
{{- end -}}

{{/*
Policy ConfigMap name. Either user-provided existingConfigMap, or one we render.
*/}}
{{- define "s4.policyConfigMapName" -}}
{{- if .Values.policy.existingConfigMap -}}
{{- .Values.policy.existingConfigMap -}}
{{- else -}}
{{- printf "%s-policy" (include "s4.fullname" .) -}}
{{- end -}}
{{- end -}}

{{/*
True (string "true") if a TLS secret will be mounted (either inline or external).
*/}}
{{- define "s4.tlsActive" -}}
{{- if and .Values.tls.enabled (or (and .Values.tls.cert .Values.tls.key) .Values.tls.existingSecret) -}}
true
{{- end -}}
{{- end -}}

{{/*
True if a policy ConfigMap will be mounted.
*/}}
{{- define "s4.policyActive" -}}
{{- if or .Values.policy.json .Values.policy.existingConfigMap -}}
true
{{- end -}}
{{- end -}}

{{/*
Ledger PVC name — "-ledger" appended to a base truncated to 56 so the
result never exceeds the 63-char DNS label limit even under
fullnameOverride (s4.fullname itself truncates at 63).
*/}}
{{- define "s4.ledgerPvcName" -}}
{{- printf "%s-ledger" (include "s4.fullname" . | trunc 56 | trimSuffix "-") -}}
{{- end }}
